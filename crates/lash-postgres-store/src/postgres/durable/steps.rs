//! An owner commit's domain writes as step programs (FIG-5936).
//!
//! A step is one of the shared statements with its values and how many rows
//! it must answer. A domain write that is a step program is decided by its
//! statements alone: its commit either runs the steps here, in the owner's
//! guarded transaction, or ships them to the one-call executor
//! ([`super::executor`]), which runs the same statements server-side. A step
//! that answers another number of rows refuses the commit, and the domain's
//! classifier, reading in the same transaction, names the refusal: only this
//! path classifies, so a refusal the executor meets is run again here.

use lash_durable::DurableError;
use lash_store_sql::Rendered;
use sqlx::PgConnection;
use sqlx::postgres::PgRow;

use super::sqlx_failure;

/// A step parameter's type, as the executor decodes it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Ty {
    Text,
    Int,
    Bytes,
    Texts,
    Bools,
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
}

/// A statement an owner commit may run as a step.
pub(super) struct Entry {
    statement: fn() -> &'static Rendered,
    pub(super) params: &'static [Ty],
    pub(super) shape: Shape,
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
        }
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
    Bytes(Option<Vec<u8>>),
    Texts(Vec<Option<String>>),
    Bools(Vec<bool>),
}

impl Value {
    fn ty(&self) -> Ty {
        match self {
            Self::Text(_) => Ty::Text,
            Self::Int(_) => Ty::Int,
            Self::Bytes(_) => Ty::Bytes,
            Self::Texts(_) => Ty::Texts,
            Self::Bools(_) => Ty::Bools,
        }
    }

    pub(super) fn text(value: impl Into<String>) -> Self {
        Self::Text(Some(value.into()))
    }

    pub(super) fn int(value: i64) -> Self {
        Self::Int(Some(value))
    }

    /// The value as the executor's JSON reads it.
    pub(super) fn json(&self) -> serde_json::Value {
        use serde_json::Value as Json;
        fn hex(bytes: &[u8]) -> String {
            bytes.iter().map(|byte| format!("{byte:02x}")).collect()
        }
        match self {
            Self::Text(text) => text.clone().map_or(Json::Null, Json::String),
            Self::Int(int) => int.map_or(Json::Null, Json::from),
            Self::Bytes(bytes) => bytes
                .as_deref()
                .map_or(Json::Null, |bytes| hex(bytes).into()),
            Self::Texts(texts) => texts
                .iter()
                .map(|text| text.clone().map_or(Json::Null, Json::String))
                .collect(),
            Self::Bools(flags) => flags.iter().copied().map(Json::Bool).collect(),
        }
    }
}

/// One statement of a step program, with its values and the rows it must
/// answer (`None`: any number).
#[derive(Clone)]
pub(super) struct Step {
    pub(super) entry: &'static Entry,
    pub(super) values: Vec<Value>,
    pub(super) expect: Option<u64>,
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
        }
    }

    /// `entry` run with `values`, refusing unless it answers one row.
    pub(super) fn one(entry: &'static Entry, values: Vec<Value>) -> Self {
        Self::new(entry, values, Some(1))
    }

    /// `entry` run with `values`, whatever it answers.
    pub(super) fn any(entry: &'static Entry, values: Vec<Value>) -> Self {
        Self::new(entry, values, None)
    }

    /// Run the step in `tx`: how many rows it answered, and the
    /// rows a query or a `RETURNING` write answered.
    async fn run(&self, tx: &mut PgConnection) -> Result<(u64, Vec<PgRow>), DurableError> {
        let mut query = crate::observed_sql::query(self.entry.statement().sql());
        for value in &self.values {
            query = match value {
                Value::Text(text) => query.bind(text.clone()),
                Value::Int(int) => query.bind(*int),
                Value::Bytes(bytes) => query.bind(bytes.clone()),
                Value::Texts(texts) => query.bind(texts.clone()),
                Value::Bools(flags) => query.bind(flags.clone()),
            };
        }
        let executor = crate::observed_sql::executor(&mut *tx);
        match self.entry.shape {
            Shape::Exec => query
                .execute(executor)
                .await
                .map(|done| (done.rows_affected(), Vec::new())),
            Shape::Returning | Shape::Select => query
                .fetch_all(executor)
                .await
                .map(|rows| (rows.len() as u64, rows)),
        }
        .map_err(sqlx_failure)
    }
}

/// The step of a program that answered another number of rows than it
/// must: its index, and the rows it answered.
pub(super) struct Refused {
    pub(super) index: usize,
    pub(super) rows: Vec<PgRow>,
}

/// Run `steps` in `tx`, in order, until one answers another number
/// of rows than it must: that step, which ends the run.
pub(super) async fn run(
    tx: &mut PgConnection,
    steps: &[Step],
) -> Result<Option<Refused>, DurableError> {
    for (index, step) in steps.iter().enumerate() {
        let (count, rows) = step.run(tx).await?;
        if step.expect.is_some_and(|expect| expect != count) {
            return Ok(Some(Refused { index, rows }));
        }
    }
    Ok(None)
}
