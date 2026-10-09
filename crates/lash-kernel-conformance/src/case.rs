//! The portable corpus envelope. Documents remain kernel text.

use std::collections::BTreeMap;

use lash_kernel_doc::{
    Datum, EffectIdentity, EffectName, ErrorDatum, Handle, Name, Signature, TaskIdentity,
    Timestamp, Type,
};
use lash_kernel_vm::{Bound, Bounds, End, Outcome, RunError};
use serde::{Deserialize, Serialize};

use crate::HarnessError;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shard {
    pub rule: String,
    pub cases: Vec<Case>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Case {
    pub name: String,
    pub document: String,
    #[serde(default)]
    pub environment: Environment,
    pub expected: Expected,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Environment {
    /// None provides the manifest's effects; Some supplies an explicit host catalogue.
    pub effects: Option<BTreeMap<EffectName, Signature>>,
    pub bounds: CaseBounds,
    pub slice: u64,
    pub max_steps: usize,
    /// Each batch is delivered at one park, in this order, before running again.
    pub deliveries: Vec<Vec<Delivery>>,
    pub host: Vec<HostAnswer>,
    /// Discard the machine and rebuild from exported state at every park.
    pub resume: bool,
    pub entry: Option<Name>,
    pub args: Vec<Datum>,
}

impl Default for Environment {
    fn default() -> Self {
        Self {
            effects: None,
            bounds: CaseBounds::default(),
            slice: u64::MAX,
            max_steps: 10_000,
            deliveries: Vec::new(),
            host: Vec::new(),
            resume: false,
            entry: None,
            args: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CaseBounds {
    pub charge: u64,
    pub memory: u64,
    pub call_depth: u32,
    pub live_tasks: u32,
    pub requests_per_park: u32,
    pub join_members: u32,
}

impl Default for CaseBounds {
    fn default() -> Self {
        Self {
            charge: 1_000_000,
            memory: 16 * 1024 * 1024,
            call_depth: 64,
            live_tasks: 128,
            requests_per_park: 128,
            join_members: 128,
        }
    }
}

impl From<CaseBounds> for Bounds {
    fn from(value: CaseBounds) -> Self {
        Self {
            charge: value.charge,
            memory: value.memory,
            call_depth: value.call_depth,
            live_tasks: value.live_tasks,
            requests_per_park: value.requests_per_park,
            join_members: value.join_members,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Delivery {
    /// Index in the complete request trace, rather than an engine's wait id.
    pub request: usize,
    pub outcome: ScriptOutcome,
    #[serde(default)]
    pub dropped: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScriptOutcome {
    Completed(Datum),
    Failed(ErrorDatum),
    Elapsed,
}

impl From<ScriptOutcome> for Outcome {
    fn from(value: ScriptOutcome) -> Self {
        match value {
            ScriptOutcome::Completed(value) => Self::Completed(value),
            ScriptOutcome::Failed(error) => Self::Failed(error),
            ScriptOutcome::Elapsed => Self::Elapsed,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostAnswer {
    Clock(Timestamp),
    Random(u64),
    Read(ReadAnswer),
    Cancel(bool),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadAnswer {
    pub handle: Handle,
    pub request: Datum,
    pub answer: Result<Datum, ErrorDatum>,
}

/// A request in issue order. Actual traces always carry their full identity;
/// an expectation may omit it when the cited rule does not pin site derivation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Trace {
    Effect {
        #[serde(default)]
        identity: Option<EffectIdentity>,
        effect: String,
        args: Vec<Datum>,
        result: Type,
    },
    Sleep {
        #[serde(default)]
        identity: Option<EffectIdentity>,
        nanoseconds: u128,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ExpectedEnd {
    Finished(Datum),
    Failed(Datum),
    Uncaught(Datum),
    TasksOutstanding {
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
    },
    Deadlock {
        waiting: Vec<TaskIdentity>,
    },
    Bound {
        name: String,
        limit: u64,
    },
    Cancelled,
    /// Structural refusal is an observation too. Runtime faults never match it.
    Refused,
}

impl ExpectedEnd {
    pub(crate) fn from_end(end: End) -> Self {
        match end {
            End::Finished(end) => Self::Finished(end.result),
            End::Failed(value) => Self::Failed(value),
            End::Cancelled => Self::Cancelled,
            End::Error(RunError::Uncaught(value)) => Self::Uncaught(value),
            End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
            }) => Self::TasksOutstanding {
                unfinished,
                unobserved,
            },
            End::Error(RunError::Deadlock { waiting }) => Self::Deadlock { waiting },
            End::Error(RunError::Bound(error)) => Self::Bound {
                name: match error.bound {
                    Bound::Charge => "charge".into(),
                    Bound::Memory => "memory".into(),
                    Bound::CallDepth => "call_depth".into(),
                    Bound::LiveTasks => "live_tasks".into(),
                    Bound::RequestsPerPark => "requests_per_park".into(),
                    Bound::JoinMembers => "join_members".into(),
                    Bound::Guard { function } => format!("guard:{function}"),
                },
                limit: error.limit,
            },
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expected {
    #[serde(default)]
    pub prints: Vec<Datum>,
    pub end: ExpectedEnd,
    #[serde(default)]
    pub trace: Vec<Trace>,
    #[serde(default)]
    pub charged: Option<u64>,
    #[serde(default)]
    pub parks: Option<usize>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observations {
    pub prints: Vec<Datum>,
    pub end: ExpectedEnd,
    pub trace: Vec<Trace>,
    pub charged: u64,
    pub parks: usize,
}

impl Expected {
    pub fn check(&self, actual: &Observations) -> Result<(), HarnessError> {
        let mut trace = actual.trace.clone();
        for (expected, seen) in self.trace.iter().zip(&mut trace) {
            match (expected, seen) {
                (Trace::Effect { identity: None, .. }, Trace::Effect { identity, .. })
                | (Trace::Sleep { identity: None, .. }, Trace::Sleep { identity, .. }) => {
                    *identity = None;
                }
                _ => {}
            }
        }
        if self.prints != actual.prints
            || self.end != actual.end
            || self.trace != trace
            || self.charged.is_some_and(|charge| charge != actual.charged)
            || self.parks.is_some_and(|parks| parks != actual.parks)
        {
            return Err(HarnessError(format!(
                "expected {self:?}, observed {actual:?}"
            )));
        }
        Ok(())
    }
}
