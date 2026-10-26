//! Exact physical execution facts. These diagnostics are never guest facts or
//! persisted state. Owner scopes describe real control handoffs (including
//! returns); statements are counted at their executor's admission boundary.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;

/// The physical executor that runs: the register program, which runs every
/// statement, or the source op evaluator, which runs a statement on from the
/// op an exit stopped at.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Owner {
    RegisterProgram,
    SourceEvaluator,
}

/// The guard or semantic boundary which hands work to a slower owner.
/// Reasons describe the actual rejecting boundary, not a sampled attribution.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reason {
    Observation,
    Capacity,
    Depth,
    Arguments,
    NotAdmitted,
    Slice,
    Charge,
    SourceContinuation,
    ResidentRead,
    Native,
    Operator,
    Call,
    Binding,
    Condition,
    ColdBranch,
    OccurrenceOrHalt,
    RegionGuard,
    Unproductive,
    RegionBoundary,
}

/// Counts for one explicitly delimited machine run. Absent map
/// entries mean zero; the enum is the only inventory of physical owners. Statements
/// are physical statement executions, not charge units: source continuations
/// do not start a second statement; speculative regions count only commits.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Census {
    pub statements: BTreeMap<Owner, u64>,
    pub transitions: BTreeMap<Owner, BTreeMap<Owner, u64>>,
    pub register_entries: u64,
    pub slow_exits: BTreeMap<Owner, BTreeMap<Reason, u64>>,
    /// The library operation at a precise exit, grouped by its exit reason.
    /// This is diagnostic execution data, never a guest or checkpoint fact.
    pub operation_exits: BTreeMap<Reason, BTreeMap<String, u64>>,
    /// The linear text accumulators' writes that committed a text their
    /// concatenation extended in place (FIG-6795).
    pub accumulated: u64,
}

struct Collector {
    census: Census,
    owner: Owner,
    last: Owner,
}

impl Default for Collector {
    fn default() -> Self {
        // Every Machine::run steps its frames in the register program. Child
        // scopes record handoffs and returns to it; idle host time is not an
        // owner.
        let owner = Owner::RegisterProgram;
        Self {
            census: Census::default(),
            owner,
            last: owner,
        }
    }
}

/// One hosted machine owns its diagnostic collection. Scopes share only this
/// instance's collector so a nested executor can return control on drop.
#[derive(Clone, Default)]
pub(crate) struct Recorder {
    inner: Option<Arc<Mutex<Collector>>>,
}

impl Recorder {
    pub(crate) fn enable(&mut self) {
        self.inner = Some(Arc::new(Mutex::new(Collector::default())));
    }

    pub(crate) fn finish(&mut self) -> Census {
        self.inner.take().map_or_else(Census::default, |counts| {
            std::mem::take(
                &mut counts
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .census,
            )
        })
    }
}

/// One canonical producer, used by all physical execution owners. Projections
/// consume its census; they never infer execution from charge or samples.
enum Fact {
    Owner(Owner),
    Statements(Owner, u64),
    RegisterEntry,
    SlowExit(Owner, Reason),
    OperationExit(Reason, String),
    Accumulated,
}

fn produce(counts: &Arc<Mutex<Collector>>, fact: Fact) {
    let mut counts = counts
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match fact {
        Fact::Owner(owner) => {
            if counts.last != owner {
                let from = counts.last;
                *counts
                    .census
                    .transitions
                    .entry(from)
                    .or_default()
                    .entry(owner)
                    .or_default() += 1;
            }
            counts.last = owner;
            counts.owner = owner;
        }
        Fact::Statements(owner, n) => {
            *counts.census.statements.entry(owner).or_default() += n;
        }
        Fact::RegisterEntry => counts.census.register_entries += 1,
        Fact::Accumulated => counts.census.accumulated += 1,
        Fact::SlowExit(owner, reason) => {
            *counts
                .census
                .slow_exits
                .entry(owner)
                .or_default()
                .entry(reason)
                .or_default() += 1;
        }
        Fact::OperationExit(reason, name) => {
            *counts
                .census
                .operation_exits
                .entry(reason)
                .or_default()
                .entry(name)
                .or_default() += 1;
        }
    }
}

/// A physical owner holds control until it returns, including early exits.
/// Constructed at the claim, before any callback can hand control elsewhere.
pub(crate) struct Scope {
    previous: Option<ScopeOwner>,
}

struct ScopeOwner {
    counts: Arc<Mutex<Collector>>,
    previous: Owner,
}
impl Scope {
    #[inline(always)]
    pub(crate) fn enter(recorder: &Recorder, owner: Owner, enabled: bool) -> Self {
        let previous = if enabled && let Some(counts) = &recorder.inner {
            let previous = counts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .owner;
            produce(counts, Fact::Owner(owner));
            Some(ScopeOwner {
                counts: Arc::clone(counts),
                previous,
            })
        } else {
            None
        };
        Self { previous }
    }
}
impl Drop for Scope {
    #[inline(always)]
    fn drop(&mut self) {
        if let Some(owner) = &self.previous {
            produce(&owner.counts, Fact::Owner(owner.previous));
        }
    }
}

#[inline(always)]
pub(crate) fn statements(recorder: &Recorder, owner: Owner, n: u64, counting: bool) {
    if counting
        && n != 0
        && let Some(counts) = &recorder.inner
    {
        produce(counts, Fact::Statements(owner, n));
    }
}

/// Reused typed kernels execute under their caller's physical owner scope.
#[inline(always)]
pub(crate) fn statements_here(recorder: &Recorder, n: u64, counting: bool) {
    if counting
        && n != 0
        && let Some(counts) = &recorder.inner
    {
        let owner = counts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owner;
        produce(counts, Fact::Statements(owner, n));
    }
}

#[inline(always)]
pub(crate) fn register_entry<const LITERAL: bool>(recorder: &Recorder, counting: bool) {
    if counting && let Some(counts) = &recorder.inner {
        if LITERAL {
            let _owner = Scope::enter(recorder, Owner::RegisterProgram, true);
            produce(counts, Fact::RegisterEntry);
            produce(counts, Fact::Statements(Owner::RegisterProgram, 1));
        } else {
            produce(counts, Fact::RegisterEntry);
        }
    }
}

#[inline(always)]
pub(crate) fn accumulated(recorder: &Recorder, counting: bool) {
    if counting && let Some(counts) = &recorder.inner {
        produce(counts, Fact::Accumulated);
    }
}

#[inline(always)]
pub(crate) fn slow_exit(recorder: &Recorder, owner: Owner, reason: Reason, counting: bool) {
    if counting && let Some(counts) = &recorder.inner {
        produce(counts, Fact::SlowExit(owner, reason));
    }
}

#[inline(always)]
pub(crate) fn operation_exit(recorder: &Recorder, reason: Reason, name: &str, counting: bool) {
    if counting && let Some(counts) = &recorder.inner {
        produce(counts, Fact::OperationExit(reason, name.to_owned()));
    }
}
