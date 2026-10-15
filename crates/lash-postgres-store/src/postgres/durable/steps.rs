//! An owner commit's domain writes as step programs (FIG-5936, FIG-5942).
//!
//! A step is one of the shared statements with its values and how many rows
//! it must answer. A domain write that is a step program is decided by its
//! statements alone: its commit either runs the steps here, in the owner's
//! guarded transaction, or ships them to the one-call executor
//! ([`super::executor`]), which runs the same statements server-side. A step
//! that answers another number of rows refuses the commit, and the domain's
//! classifier, reading in the same transaction, names the refusal: only this
//! path classifies, so a refusal the executor meets is run again here.
//!
//! A step can retire graph roots (FIG-5949): the roots its statement
//! answers in a named column, and the roots its writer knew it replaced.
//! Either path holds them for the retirement steps once every step ran,
//! before the commit ends.
//!
//! A program decides with its statements' answers, not with values its
//! client read (FIG-5942). A step can capture the first row it answered in
//! a slot, and a later step can run only when that slot holds a row, or a
//! row whose named boolean column is true (or not). A flagged entry counts
//! only the rows whose flag column is true, so a query answering whether a
//! fact holds is a guard. A step also collects what its rows name for the
//! commit's mechanics: the session graph leaves whose unreachable ancestry
//! is retired right after it, the actors it woke, and the prompt texts it
//! released or keeps, which a later reclaim step peels. The database clock
//! is a value no client knows before its commit: a step names it as
//! [`Value::Now`].

use std::collections::{BTreeMap, BTreeSet};

use lash_durable::{ActorKey, DurableError, DurableInstant};
use lash_store_sql::Rendered;
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use super::{Retired, sqlx_failure};

/// Why a program's run failed, before either store's error names it.
pub(super) enum Failure {
    Sql(sqlx::Error),
    Store(crate::StoreError),
    Durable(DurableError),
}

impl From<sqlx::Error> for Failure {
    fn from(error: sqlx::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<Failure> for DurableError {
    fn from(failure: Failure) -> Self {
        match failure {
            Failure::Sql(error) => sqlx_failure(error),
            Failure::Store(error) => super::store_failure(error),
            Failure::Durable(error) => error,
        }
    }
}

impl Failure {
    /// The failure as the session store reports it.
    pub(super) fn into_store(self) -> crate::StoreError {
        match self {
            Self::Sql(error) => crate::support::store_sqlx_error(error),
            Self::Store(error) => error,
            Self::Durable(error) => crate::StoreError::Backend(error.to_string()),
        }
    }
}

/// A step parameter's type, as the executor decodes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ty {
    Text,
    Int,
    Bool,
    Bytes,
    Texts,
    Bools,
    Ints,
    /// The commit's database clock, in milliseconds: no value is sent.
    Now,
}

/// What a step's statement answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Shape {
    /// A write without `RETURNING`: its rows are the rows it wrote.
    Exec,
    /// A write with `RETURNING`: its rows are the rows it returned.
    Returning,
    /// A query: its rows are the rows it answered.
    Select,
    /// The artifact local end (FIG-5971): its one row is the answer this
    /// path validates. The one-call commit vouches for the answer instead,
    /// and declines one it cannot vouch for ([`super::executor`]).
    EndLocal,
    /// The reclaim of the prompt texts the earlier steps released, keeping
    /// those they protected. It answers no rows.
    TextReclaim,
}

/// What an answered column names for the commit's mechanics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Collect {
    /// A graph root the row retired: its closure is retired.
    Closure(&'static str),
    /// An array of graph roots the row retired.
    Closures(&'static str),
    /// An ended turn's PhaseRoot: retired alone.
    PhaseEnd(&'static str),
    /// A session graph leaf whose unreachable ancestry is retired.
    Ancestry(&'static str),
    /// An actor the row woke, unless it is the committing one.
    Wake(&'static str),
    /// A prompt text the row released.
    Text(&'static str),
    /// A prompt text the reclaim keeps.
    Protect(&'static str),
}

impl Collect {
    pub(super) fn column(self) -> &'static str {
        match self {
            Self::Closure(column)
            | Self::Closures(column)
            | Self::PhaseEnd(column)
            | Self::Ancestry(column)
            | Self::Wake(column)
            | Self::Text(column)
            | Self::Protect(column) => column,
        }
    }
}

/// The most collections one entry names.
const COLLECTS: usize = 3;

/// A statement an owner commit may run as a step.
pub(super) struct Entry {
    statement: fn() -> &'static Rendered,
    pub(super) params: &'static [Ty],
    pub(super) shape: Shape,
    /// The boolean column whose true rows are the step's rows.
    pub(super) flag: Option<&'static str>,
    collects: [Option<Collect>; COLLECTS],
}

impl Entry {
    pub(super) const fn new(
        statement: fn() -> &'static Rendered,
        params: &'static [Ty],
        shape: Shape,
    ) -> Self {
        Self {
            statement,
            params,
            shape,
            flag: None,
            collects: [None; COLLECTS],
        }
    }

    /// This entry, each answered row of which retires the root its column
    /// `column` names (none when null): a query or a `RETURNING` write.
    pub(super) const fn retiring(self, column: &'static str) -> Self {
        self.collecting(Collect::Closure(column))
    }

    /// This entry, whose answered rows also name `collect`: a query or a
    /// `RETURNING` write.
    pub(super) const fn collecting(mut self, collect: Collect) -> Self {
        assert!(matches!(self.shape, Shape::Returning | Shape::Select));
        let mut index = 0;
        while index < COLLECTS {
            if self.collects[index].is_none() {
                self.collects[index] = Some(collect);
                return self;
            }
            index += 1;
        }
        panic!("an entry names at most three collections");
    }

    /// This entry, whose rows are those whose boolean column `column` is
    /// true: a query or a `RETURNING` write.
    pub(super) const fn flagged(mut self, column: &'static str) -> Self {
        assert!(matches!(self.shape, Shape::Returning | Shape::Select));
        self.flag = Some(column);
        self
    }

    /// What the answered rows name for the commit's mechanics.
    pub(super) fn collects(&self) -> impl Iterator<Item = Collect> + '_ {
        self.collects.iter().flatten().copied()
    }

    pub(super) fn statement(&self) -> &'static Rendered {
        (self.statement)()
    }

    /// The step's name: its statement's.
    pub(super) fn name(&self) -> &'static str {
        self.statement().name()
    }
}

/// A step parameter's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Value {
    Text(Option<String>),
    Int(Option<i64>),
    Bool(bool),
    Bytes(Option<Vec<u8>>),
    Texts(Vec<Option<String>>),
    Bools(Vec<bool>),
    Ints(Vec<Option<i64>>),
    /// The commit's database clock.
    Now,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Value {
    fn ty(&self) -> Ty {
        match self {
            Self::Text(_) => Ty::Text,
            Self::Int(_) => Ty::Int,
            Self::Bool(_) => Ty::Bool,
            Self::Bytes(_) => Ty::Bytes,
            Self::Texts(_) => Ty::Texts,
            Self::Bools(_) => Ty::Bools,
            Self::Ints(_) => Ty::Ints,
            Self::Now => Ty::Now,
        }
    }

    pub(super) fn text(value: impl Into<String>) -> Self {
        Self::Text(Some(value.into()))
    }

    pub(super) fn int(value: i64) -> Self {
        Self::Int(Some(value))
    }

    /// Each of `values`, as a list of texts.
    pub(super) fn texts<T: AsRef<str>>(values: impl IntoIterator<Item = T>) -> Self {
        Self::Texts(
            values
                .into_iter()
                .map(|value| Some(value.as_ref().to_owned()))
                .collect(),
        )
    }

    /// The value as the executor's JSON reads it.
    pub(super) fn json(&self) -> serde_json::Value {
        use serde_json::Value as Json;
        match self {
            Self::Text(text) => text.clone().map_or(Json::Null, Json::String),
            Self::Int(int) => int.map_or(Json::Null, Json::from),
            Self::Bool(flag) => Json::Bool(*flag),
            Self::Bytes(bytes) => bytes
                .as_deref()
                .map_or(Json::Null, |bytes| hex(bytes).into()),
            Self::Texts(texts) => texts
                .iter()
                .map(|text| text.clone().map_or(Json::Null, Json::String))
                .collect(),
            Self::Bools(flags) => flags.iter().copied().map(Json::Bool).collect(),
            Self::Ints(ints) => ints
                .iter()
                .map(|int| int.map_or(Json::Null, Json::from))
                .collect(),
            Self::Now => Json::Null,
        }
    }
}

/// When a step runs: slot `slot` holds a row (`column` `None`), or a row
/// whose boolean column `column` is true, exactly when `holds`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct When {
    pub(super) slot: u8,
    pub(super) column: Option<&'static str>,
    pub(super) holds: bool,
}

/// One statement of a step program, with its values, the rows it must
/// answer (`None`: any number) and the roots it retires once it does.
#[derive(Clone)]
pub(super) struct Step {
    pub(super) entry: &'static Entry,
    pub(super) values: Vec<Value>,
    pub(super) expect: Option<u64>,
    pub(super) retired: Vec<String>,
    /// Session graph leaves whose unreachable ancestry the step retires.
    pub(super) ancestry: Vec<String>,
    /// The slot that records the first row the step answered.
    pub(super) capture: Option<u8>,
    /// What must hold for the step to run.
    pub(super) when: Option<When>,
}

impl Step {
    /// `entry` run with `values`, refusing unless it answers `expect` rows.
    pub(super) fn new(entry: &'static Entry, values: Vec<Value>, expect: Option<u64>) -> Self {
        debug_assert!(
            values
                .iter()
                .map(Value::ty)
                .eq(entry.params.iter().copied()),
            "step {} binds {values:?} for {:?}",
            entry.name(),
            entry.params
        );
        Self {
            entry,
            values,
            expect,
            retired: Vec::new(),
            ancestry: Vec::new(),
            capture: None,
            when: None,
        }
    }

    /// This step, which retires `roots` once it answers what it must.
    pub(super) fn retiring(mut self, roots: impl IntoIterator<Item = String>) -> Self {
        self.retired.extend(roots);
        self
    }

    /// This step, after which the unreachable ancestry of `leaves` is
    /// retired.
    pub(super) fn retiring_ancestry(mut self, leaves: impl IntoIterator<Item = String>) -> Self {
        self.ancestry.extend(leaves);
        self
    }

    /// This step, which records the first row it answers in `slot`.
    pub(super) fn capturing(mut self, slot: u8) -> Self {
        self.capture = Some(slot);
        self
    }

    /// This step, which runs only when `slot` holds a row exactly when
    /// `holds`.
    pub(super) fn when_row(mut self, slot: u8, holds: bool) -> Self {
        self.when = Some(When {
            slot,
            column: None,
            holds,
        });
        self
    }

    /// This step, which runs only when `slot` holds a row whose `column`
    /// is true exactly when `holds`.
    pub(super) fn when_flag(mut self, slot: u8, column: &'static str, holds: bool) -> Self {
        self.when = Some(When {
            slot,
            column: Some(column),
            holds,
        });
        self
    }

    /// `entry` run with `values`, refusing unless it answers one row.
    pub(super) fn one(entry: &'static Entry, values: Vec<Value>) -> Self {
        Self::new(entry, values, Some(1))
    }

    /// `entry` run with `values`, refusing unless it answers no row.
    pub(super) fn none(entry: &'static Entry, values: Vec<Value>) -> Self {
        Self::new(entry, values, Some(0))
    }

    /// `entry` run with `values`, whatever it answers.
    pub(super) fn any(entry: &'static Entry, values: Vec<Value>) -> Self {
        Self::new(entry, values, None)
    }

    /// Run the step's statement in `tx` at `now`: how many rows it
    /// answered (its true rows, when its entry is flagged), and the rows a
    /// query or a `RETURNING` write answered.
    async fn run(
        &self,
        tx: &mut PgConnection,
        now: DurableInstant,
    ) -> Result<(u64, Vec<PgRow>), Failure> {
        use sqlx::Row as _;
        let mut query = crate::observed_sql::query(self.entry.statement().sql());
        // An end's vouching values are the executor's alone.
        let bound = match self.entry.shape {
            Shape::EndLocal => END_LOCAL_PARAMS,
            _ => self.values.len(),
        };
        for value in &self.values[..bound] {
            query = match value {
                Value::Text(text) => query.bind(text.clone()),
                Value::Int(int) => query.bind(*int),
                Value::Bool(flag) => query.bind(*flag),
                Value::Bytes(bytes) => query.bind(bytes.clone()),
                Value::Texts(texts) => query.bind(texts.clone()),
                Value::Bools(flags) => query.bind(flags.clone()),
                Value::Ints(ints) => query.bind(ints.clone()),
                Value::Now => query.bind(now.0),
            };
        }
        let executor = crate::observed_sql::executor(&mut *tx);
        Ok(match self.entry.shape {
            Shape::Exec => query
                .execute(executor)
                .await
                .map(|done| (done.rows_affected(), Vec::new())),
            Shape::EndLocal => {
                let rows = query.fetch_all(executor).await.map_err(|error| {
                    Failure::Store(crate::artifact_store::end_local_failure(error))
                })?;
                for row in &rows {
                    let answer: serde_json::Value = row.try_get(0)?;
                    crate::artifact_store::validate_local_end(answer).map_err(Failure::Store)?;
                }
                return Ok((rows.len() as u64, rows));
            }
            Shape::Returning | Shape::Select => query.fetch_all(executor).await.and_then(|rows| {
                let Some(flag) = self.entry.flag else {
                    return Ok((rows.len() as u64, rows));
                };
                let mut truths = 0;
                for row in &rows {
                    if row.try_get::<Option<bool>, _>(flag)? == Some(true) {
                        truths += 1;
                    }
                }
                Ok((truths, rows))
            }),
            Shape::TextReclaim => unreachable!("a reclaim runs no statement of its own"),
        }?)
    }
}

/// How many parameters the local end's statement takes; an end step's
/// further values are what a one-call commit vouches for.
pub(super) const END_LOCAL_PARAMS: usize = 6;

/// What a program runs under: the roots it retires, the committing actor a
/// wake skips, and the commit's clock.
pub(super) struct Context<'a> {
    pub(super) retired: &'a Retired,
    pub(super) actor: &'a ActorKey,
    pub(super) now: DurableInstant,
}

/// The step of a program that answered another number of rows than it
/// must: its index, and the rows it answered.
pub(super) struct Refused {
    pub(super) index: usize,
    pub(super) rows: Vec<PgRow>,
}

/// What a slot captured: no row, or a row's boolean columns that a later
/// step's condition names.
pub(super) type Slots = BTreeMap<u8, Option<BTreeMap<&'static str, bool>>>;

/// Whether `when` holds over `slots`.
fn holds(slots: &Slots, when: &When) -> bool {
    let captured = slots.get(&when.slot).and_then(Option::as_ref);
    let truth = match (captured, when.column) {
        (None, _) => false,
        (Some(_), None) => true,
        (Some(row), Some(column)) => row.get(column).copied().unwrap_or(false),
    };
    truth == when.holds
}

/// Run `steps` in `tx`, in order, until one answers another number of rows
/// than it must: that step, which ends the run. Each step that ran records
/// the roots it retired in the context's `retired`, retires the ancestry it
/// named, wakes the actors it named and collects the texts a later reclaim
/// peels.
pub(super) async fn run(
    tx: &mut PgConnection,
    steps: &[Step],
    context: &Context<'_>,
) -> Result<Option<Refused>, DurableError> {
    Ok(run_capturing(tx, steps, context).await?.err())
}

/// [`run`], answering what the slots captured when no step refused.
pub(super) async fn run_capturing(
    tx: &mut PgConnection,
    steps: &[Step],
    context: &Context<'_>,
) -> Result<Result<Slots, Refused>, Failure> {
    use sqlx::Row as _;
    // Each slot keeps the columns a later condition names.
    let mut named: BTreeMap<u8, BTreeSet<&'static str>> = BTreeMap::new();
    for step in steps {
        if let Some(When {
            slot,
            column: Some(column),
            ..
        }) = step.when
        {
            named.entry(slot).or_default().insert(column);
        }
    }
    let mut slots = Slots::new();
    let mut texts = BTreeSet::new();
    let mut protected = BTreeSet::new();
    for (index, step) in steps.iter().enumerate() {
        if let Some(when) = &step.when
            && !holds(&slots, when)
        {
            continue;
        }
        if step.entry.shape == Shape::TextReclaim {
            let released = std::mem::take(&mut texts);
            let kept = std::mem::take(&mut protected);
            super::prompts::reclaim_with_protection(tx, released.into_iter().collect(), &kept)
                .await?;
            continue;
        }
        let (count, rows) = step.run(tx, context.now).await?;
        if let Some(slot) = step.capture {
            let captured = match rows.first() {
                Some(row) => {
                    let mut columns = BTreeMap::new();
                    for column in named.get(&slot).into_iter().flatten() {
                        let truth: Option<bool> = row.try_get(*column)?;
                        columns.insert(*column, truth.unwrap_or(false));
                    }
                    Some(columns)
                }
                None if step.entry.shape == Shape::Exec && count > 0 => Some(BTreeMap::new()),
                None => None,
            };
            slots.insert(slot, captured);
        }
        if step.expect.is_some_and(|expect| expect != count) {
            return Ok(Err(Refused { index, rows }));
        }
        let mut leaves: BTreeSet<String> = step.ancestry.iter().cloned().collect();
        let mut owners = Vec::new();
        for collect in step.entry.collects() {
            let column = collect.column();
            for row in &rows {
                if let Collect::Closures(_) = collect {
                    let roots: Option<Vec<String>> = row.try_get(column)?;
                    for root in roots.unwrap_or_default() {
                        context.retired.stored(root);
                    }
                    continue;
                }
                let Some(value) = row.try_get::<Option<String>, _>(column)? else {
                    continue;
                };
                match collect {
                    Collect::Closure(_) => context.retired.stored(value),
                    Collect::PhaseEnd(_) => context.retired.phase_end(value),
                    Collect::Ancestry(_) => {
                        leaves.insert(value);
                    }
                    Collect::Wake(_) => owners.push(value),
                    Collect::Text(_) => {
                        texts.insert(value);
                    }
                    Collect::Protect(_) => {
                        protected.insert(value);
                    }
                    Collect::Closures(_) => {}
                }
            }
        }
        for root in &step.retired {
            context.retired.stored(root.clone());
        }
        // Each leaf in id order: the order every multi-node writer locks in.
        for leaf in &leaves {
            crate::runtime_persistence::retire_unreachable_ancestry_tx(tx, leaf)
                .await
                .map_err(Failure::Store)?;
        }
        if !owners.is_empty() {
            super::waits::wake_owners(tx, context.actor, owners, context.now)
                .await
                .map_err(Failure::Durable)?;
        }
    }
    Ok(Ok(slots))
}
