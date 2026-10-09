//! The ledger of one kernel run: the set of waits the machine stands on,
//! keyed by the machine's effect identity (`K-EFF-008`), and the checkpoint
//! that commits it with the saved state it matches.
//!
//! A run is a set of tasks, so it waits on a set of effects. Each entry is
//! one wait the saved state holds, in the standing its park gave it: an
//! admitted execution, a sleep with the deadline it was admitted with, or a
//! refusal. Whether an admitted effect's outcome has committed is read from
//! its run records, never stored here: the records are the one authority.
//!
//! # What stays reachable
//!
//! The ledger a park saves names every effect the saved state still waits
//! on. An outcome delivered to the machine leaves the live ledger at once,
//! but the saved one still names it until the next save, so after a crash
//! the outcome is delivered again from its record. A park's run records
//! therefore stay until a saved ledger names none of its effects, neither
//! awaited nor [released](EffectLedger::released): the save that drops the
//! last of them prunes the records in its own transaction
//! ([`EffectLedger::oldest_reachable`]).

use std::collections::BTreeMap;

use lash_durable::DurableInstant;
use lash_kernel_doc::{
    Datum, EffectIdentity, EffectName, ErrorDatum, FunctionId, FunctionName, Name, Object,
    ObjectId, TaskIdentity, Value,
};
use lash_kernel_vm::{Bindings, Bound, BoundExceeded, End, Finished, RunError, WaitId};
use lash_vm_protocol::EncodedPayload;
use serde::{Deserialize, Serialize};

use crate::snapshot::OperationId;

/// An admitted execution the ledger keeps until it settles: its identity,
/// its call, and the request its body is built from again on any owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdmittedEffect {
    /// Minted when its park committed: the park and its ordinal there.
    pub operation: OperationId,
    pub call: lash_sansio::ToolCallId,
    pub request: EncodedPayload,
}

/// How a wait stands once its park committed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Standing {
    /// An admitted execution, run through the admitted-execution lifecycle.
    Admitted(AdmittedEffect),
    /// A sleep, over at the instant its park pinned.
    Sleeping { until_ms: i64 },
    /// The parent refused the effect: it is answered with this error and
    /// nothing was dispatched.
    Refused(ErrorDatum),
}

/// One wait the saved state stands on.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingEffect {
    /// The machine's number for the wait, which its outcome is delivered
    /// to.
    pub wait: u64,
    /// The effect performed; none for a sleep.
    pub effect: Option<EffectName>,
    pub standing: Standing,
}

impl PendingEffect {
    pub fn wait(&self) -> WaitId {
        WaitId(self.wait)
    }
}

/// The broker's state that commits with a run's saved state.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "DecodedLedger")]
pub struct EffectLedger {
    /// The waits the state stands on, by effect identity.
    #[serde(with = "entries")]
    pending: BTreeMap<EffectIdentity, PendingEffect>,
    /// Admitted executions the run no longer waits on (their task was
    /// cancelled) that are not known to have settled. They stay live under
    /// the run, as a join's losers do (ADR 0065), and the run's end settles
    /// each one.
    released: Vec<AdmittedEffect>,
    /// The number the next park that admits takes.
    next_park: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DecodedLedger {
    #[serde(with = "entries")]
    pending: BTreeMap<EffectIdentity, PendingEffect>,
    released: Vec<AdmittedEffect>,
    next_park: u64,
}

/// A stored ledger that names what no park of its run admitted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LedgerRefusal {
    #[error("the ledger names an execution of park {park}, and its run has admitted {parks}")]
    UnissuedPark { park: u64, parks: u64 },
    #[error("two of the ledger's waits share wait {wait}")]
    SharedWait { wait: u64 },
    #[error("two of the ledger's entries share execution {operation:?}")]
    SharedExecution { operation: OperationId },
}

impl TryFrom<DecodedLedger> for EffectLedger {
    type Error = LedgerRefusal;

    fn try_from(decoded: DecodedLedger) -> Result<Self, Self::Error> {
        let ledger = Self {
            pending: decoded.pending,
            released: decoded.released,
            next_park: decoded.next_park,
        };
        let mut waits: Vec<u64> = ledger.pending.values().map(|entry| entry.wait).collect();
        waits.sort_unstable();
        if let Some(pair) = waits.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(LedgerRefusal::SharedWait { wait: pair[0] });
        }
        let mut operations: Vec<OperationId> = ledger.executions().map(|e| e.operation).collect();
        operations.sort_unstable();
        if let Some(pair) = operations.windows(2).find(|pair| pair[0] == pair[1]) {
            return Err(LedgerRefusal::SharedExecution { operation: pair[0] });
        }
        if let Some(operation) = operations.iter().find(|o| o.run >= ledger.next_park) {
            return Err(LedgerRefusal::UnissuedPark {
                park: operation.run,
                parks: ledger.next_park,
            });
        }
        Ok(ledger)
    }
}

/// The pending map as a list of entries: its keys are not strings, so a
/// JSON checkpoint carries it as pairs.
mod entries {
    use std::collections::BTreeMap;

    use lash_kernel_doc::EffectIdentity;
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use super::PendingEffect;

    pub(super) fn serialize<S: Serializer>(
        pending: &BTreeMap<EffectIdentity, PendingEffect>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        pending.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<EffectIdentity, PendingEffect>, D::Error> {
        let pairs = Vec::<(EffectIdentity, PendingEffect)>::deserialize(deserializer)?;
        let count = pairs.len();
        let pending: BTreeMap<_, _> = pairs.into_iter().collect();
        if pending.len() != count {
            return Err(serde::de::Error::custom(
                "two pending effects share an identity",
            ));
        }
        Ok(pending)
    }
}

impl EffectLedger {
    /// The ledger of a run that has parked nowhere.
    pub fn new() -> Self {
        Self::default()
    }

    /// The waits the state stands on, in identity order.
    pub fn pending(&self) -> impl Iterator<Item = (&EffectIdentity, &PendingEffect)> {
        self.pending.iter()
    }

    pub fn get(&self, identity: &EffectIdentity) -> Option<&PendingEffect> {
        self.pending.get(identity)
    }

    /// The executions the run no longer waits on, still open.
    pub fn released(&self) -> &[AdmittedEffect] {
        &self.released
    }

    /// The number the next park that admits takes.
    pub fn next_park(&self) -> u64 {
        self.next_park
    }

    /// Every admitted execution the ledger names, awaited or released.
    pub fn executions(&self) -> impl Iterator<Item = &AdmittedEffect> {
        self.pending
            .values()
            .filter_map(|entry| match &entry.standing {
                Standing::Admitted(admitted) => Some(admitted),
                Standing::Sleeping { .. } | Standing::Refused(_) => None,
            })
            .chain(&self.released)
    }

    /// Whether the state waits on an admitted execution: what a cancel
    /// settles. Without one, a cancelled run stands on sleeps alone.
    pub fn awaits_execution(&self) -> bool {
        self.pending
            .values()
            .any(|entry| matches!(entry.standing, Standing::Admitted(_)))
    }

    /// The earliest instant a sleep the state stands on is over.
    pub fn next_wake(&self) -> Option<DurableInstant> {
        self.pending
            .values()
            .filter_map(|entry| match entry.standing {
                Standing::Sleeping { until_ms } => Some(DurableInstant(until_ms)),
                Standing::Admitted(_) | Standing::Refused(_) => None,
            })
            .min()
    }

    /// The oldest park whose run records a state saved with this ledger can
    /// still read: that of the oldest execution it names, or the next
    /// park's. Every record before it is unreachable.
    pub fn oldest_reachable(&self) -> u64 {
        self.executions()
            .map(|admitted| admitted.operation.run)
            .min()
            .unwrap_or(self.next_park)
    }

    /// The park the next admission is, taken.
    pub(crate) fn take_park(&mut self) -> u64 {
        let park = self.next_park;
        self.next_park += 1;
        park
    }

    /// Stands the state on `identity` as `entry`. `false`, and nothing
    /// changes, when the ledger already names that identity: a machine
    /// never requests one effect twice.
    pub(crate) fn stand(&mut self, identity: EffectIdentity, entry: PendingEffect) -> bool {
        match self.pending.entry(identity) {
            std::collections::btree_map::Entry::Occupied(_) => false,
            std::collections::btree_map::Entry::Vacant(vacant) => {
                vacant.insert(entry);
                true
            }
        }
    }

    /// The outcome of `identity` was delivered: the live state no longer
    /// stands on it.
    pub fn consume(&mut self, identity: &EffectIdentity) -> Option<PendingEffect> {
        self.pending.remove(identity)
    }

    /// The machine withdrew `wait` (its task was cancelled, or the run
    /// moved past it): the run stops waiting on it. An admitted execution
    /// is released, live until it settles; a sleep or a refusal is gone.
    /// `false` when the ledger stands on no such wait.
    pub fn withdraw(&mut self, wait: WaitId) -> bool {
        let Some(identity) = self
            .pending
            .iter()
            .find(|(_, entry)| entry.wait == wait.0)
            .map(|(identity, _)| identity.clone())
        else {
            return false;
        };
        if let Some(PendingEffect {
            standing: Standing::Admitted(admitted),
            ..
        }) = self.pending.remove(&identity)
        {
            self.released.push(admitted);
        }
        true
    }

    /// The run ended and every execution it admitted has settled: the
    /// ledger names nothing.
    pub(crate) fn close(&mut self) {
        self.pending.clear();
        self.released.clear();
    }

    /// Forgets every released execution `settled` answers for: its outcome
    /// committed, and nothing waits for it.
    pub(crate) fn drop_released(&mut self, mut settled: impl FnMut(&AdmittedEffect) -> bool) {
        self.released.retain(|admitted| !settled(admitted));
    }
}

/// How a run ended, as its last checkpoint records it: a restore answers it
/// without starting a machine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedEnd {
    Finished {
        result: Datum,
        /// Whether a `finish` ended the run.
        finish: bool,
        variables: BTreeMap<Name, Value>,
        objects: Vec<(ObjectId, Object)>,
        not_carried: Vec<Name>,
    },
    Failed {
        reason: Datum,
    },
    Uncaught {
        error: Datum,
    },
    TasksOutstanding {
        unfinished: Vec<TaskIdentity>,
        unobserved: Vec<TaskIdentity>,
    },
    Deadlock {
        waiting: Vec<TaskIdentity>,
    },
    Bound {
        bound: RecordedBound,
        limit: u64,
        function: Option<FunctionName>,
    },
}

/// Which bound a recorded end passed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum RecordedBound {
    Charge,
    Memory,
    CallDepth,
    LiveTasks,
    RequestsPerPark,
    JoinMembers,
    Guard { function: FunctionId },
}

impl RecordedEnd {
    /// `end` as a checkpoint records it; `None` for a cancelled run, which
    /// records no end.
    pub fn of(end: &End) -> Option<Self> {
        Some(match end {
            End::Finished(finished) => Self::Finished {
                result: finished.result.clone(),
                finish: finished.finish,
                variables: finished.bindings.variables.clone(),
                objects: finished
                    .bindings
                    .objects
                    .iter()
                    .map(|(id, object)| (*id, object.clone()))
                    .collect(),
                not_carried: finished.not_carried.clone(),
            },
            End::Failed(reason) => Self::Failed {
                reason: reason.clone(),
            },
            End::Error(RunError::Uncaught(error)) => Self::Uncaught {
                error: error.clone(),
            },
            End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
            }) => Self::TasksOutstanding {
                unfinished: unfinished.clone(),
                unobserved: unobserved.clone(),
            },
            End::Error(RunError::Deadlock { waiting }) => Self::Deadlock {
                waiting: waiting.clone(),
            },
            End::Error(RunError::Bound(exceeded)) => Self::Bound {
                bound: match &exceeded.bound {
                    Bound::Charge => RecordedBound::Charge,
                    Bound::Memory => RecordedBound::Memory,
                    Bound::CallDepth => RecordedBound::CallDepth,
                    Bound::LiveTasks => RecordedBound::LiveTasks,
                    Bound::RequestsPerPark => RecordedBound::RequestsPerPark,
                    Bound::JoinMembers => RecordedBound::JoinMembers,
                    Bound::Guard { function } => RecordedBound::Guard {
                        function: *function,
                    },
                },
                limit: exceeded.limit,
                function: exceeded.function.clone(),
            },
            End::Cancelled => return None,
        })
    }

    /// The end the machine reported.
    pub fn into_end(self) -> End {
        match self {
            Self::Finished {
                result,
                finish,
                variables,
                objects,
                not_carried,
            } => End::Finished(Finished {
                result,
                finish,
                bindings: Bindings {
                    variables,
                    objects: objects.into_iter().collect(),
                },
                not_carried,
            }),
            Self::Failed { reason } => End::Failed(reason),
            Self::Uncaught { error } => End::Error(RunError::Uncaught(error)),
            Self::TasksOutstanding {
                unfinished,
                unobserved,
            } => End::Error(RunError::TasksOutstanding {
                unfinished,
                unobserved,
            }),
            Self::Deadlock { waiting } => End::Error(RunError::Deadlock { waiting }),
            Self::Bound {
                bound,
                limit,
                function,
            } => End::Error(RunError::Bound(BoundExceeded {
                bound: match bound {
                    RecordedBound::Charge => Bound::Charge,
                    RecordedBound::Memory => Bound::Memory,
                    RecordedBound::CallDepth => Bound::CallDepth,
                    RecordedBound::LiveTasks => Bound::LiveTasks,
                    RecordedBound::RequestsPerPark => Bound::RequestsPerPark,
                    RecordedBound::JoinMembers => Bound::JoinMembers,
                    RecordedBound::Guard { function } => Bound::Guard { function },
                },
                limit,
                function,
            })),
        }
    }
}

/// A run's saved state, the ledger that matches it and the host's own
/// state for the run: committed together or not at all.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParkedCheckpoint<P> {
    /// The machine's parked state; none once the run ended.
    pub state: Option<P>,
    pub ledger: EffectLedger,
    /// The host's own state for the run at this save, opaque to the broker.
    pub host: Option<EncodedPayload>,
    /// How the run ended, once it ended.
    pub end: Option<RecordedEnd>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use lash_kernel_doc::{Site, Unit};

    use super::*;

    /// `K-ERR-003`, ADR 0132 §8: restoring a recorded end returns the
    /// original thrown value, including a value that is not an error.
    #[test]
    fn a_recorded_uncaught_end_keeps_the_thrown_value() {
        let end = End::Error(RunError::Uncaught(Datum::Int(7.into())));
        let recorded = RecordedEnd::of(&end).expect("an uncaught end is recorded");
        let stored = serde_json::to_value(&recorded).expect("the end encodes");
        assert_eq!(
            stored,
            serde_json::json!({"uncaught": {"error": {"int": "7"}}})
        );
        let restored: RecordedEnd = serde_json::from_value(stored).expect("the end decodes");
        assert_eq!(restored.into_end(), end);
    }

    fn identity(occurrence: u64) -> EffectIdentity {
        EffectIdentity {
            task: TaskIdentity::Main,
            site: Site {
                unit: Unit::Main,
                path: vec![0],
            },
            occurrence,
            loops: Vec::new(),
        }
    }

    fn admitted(run: u64, ordinal: u64) -> AdmittedEffect {
        AdmittedEffect {
            operation: OperationId { run, ordinal },
            call: crate::CodeCallIdentities::cell(
                lash_core_store::effect_opener::EffectOpener::turn("session", "turn"),
                "cell",
            )
            .child_call_id(run, ordinal),
            request: EncodedPayload(Vec::new()),
        }
    }

    fn standing(wait: u64, standing: Standing) -> PendingEffect {
        PendingEffect {
            wait,
            effect: None,
            standing,
        }
    }

    /// The records of a park stay reachable while a saved ledger names any
    /// of its executions, awaited or released, and no longer.
    #[test]
    fn a_parks_records_are_reachable_until_no_entry_names_them() {
        let mut ledger = EffectLedger::new();
        assert_eq!(ledger.take_park(), 0);
        assert!(ledger.stand(identity(0), standing(0, Standing::Admitted(admitted(0, 0)))));
        assert!(ledger.stand(identity(1), standing(1, Standing::Admitted(admitted(0, 1)))));
        assert_eq!(ledger.take_park(), 1);
        assert!(ledger.stand(identity(2), standing(2, Standing::Admitted(admitted(1, 0)))));
        assert!(ledger.stand(
            identity(3),
            standing(3, Standing::Sleeping { until_ms: 40 })
        ));
        assert!(
            !ledger.stand(identity(3), standing(9, Standing::Sleeping { until_ms: 1 })),
            "one identity is requested once"
        );
        assert_eq!(ledger.oldest_reachable(), 0);
        assert_eq!(ledger.next_wake(), Some(DurableInstant(40)));

        // Delivered: park 0 is still named by its other effect.
        assert!(ledger.consume(&identity(0)).is_some());
        assert_eq!(ledger.oldest_reachable(), 0);
        // Withdrawn: the execution is released, live, and still reachable.
        assert!(ledger.withdraw(WaitId(1)));
        assert!(!ledger.withdraw(WaitId(1)));
        assert_eq!(ledger.released(), [admitted(0, 1)]);
        assert_eq!(ledger.oldest_reachable(), 0);
        // Settled: nothing names park 0.
        ledger.drop_released(|_| true);
        assert_eq!(ledger.oldest_reachable(), 1);
        assert!(ledger.withdraw(WaitId(3)), "a withdrawn sleep is gone");
        assert!(ledger.consume(&identity(2)).is_some());
        assert_eq!(ledger.oldest_reachable(), 2);
        assert_eq!(ledger.next_wake(), None);
    }

    /// A stored ledger that names an execution no park of its run admitted,
    /// or one wait twice, is refused when it is read.
    #[test]
    fn a_stored_ledger_that_names_what_was_never_admitted_is_refused() {
        let mut ledger = EffectLedger::new();
        ledger.take_park();
        ledger.stand(identity(0), standing(0, Standing::Admitted(admitted(0, 0))));
        let stored = serde_json::to_value(&ledger).expect("a ledger encodes");
        assert_eq!(
            serde_json::from_value::<EffectLedger>(stored.clone()).expect("it reads back"),
            ledger
        );
        let mut unissued = stored.clone();
        unissued["next_park"] = 0.into();
        assert!(serde_json::from_value::<EffectLedger>(unissued).is_err());
        let mut shared = stored;
        let entry = shared["pending"][0].clone();
        shared["pending"]
            .as_array_mut()
            .expect("entries")
            .push(serde_json::json!([identity(1), entry[1]]));
        assert!(serde_json::from_value::<EffectLedger>(shared).is_err());
    }
}
