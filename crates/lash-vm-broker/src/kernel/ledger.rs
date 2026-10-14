//! The ledger of one kernel run: the set of waits the machine stands on,
//! keyed by the machine's effect identity (`K-EFF-008`).
//!
//! A run is a set of tasks, so it waits on a set of effects. Each entry is
//! one wait the saved state holds, in the standing its park gave it: an
//! admitted execution, a sleep with the deadline it was admitted with, or a
//! refusal. Whether an admitted effect's outcome has committed is read from
//! its run records, never stored here: the records are the one authority.
//! A park stores the ledger as its root's effect membership, each entry by
//! the one admission that holds its request ([`super::graph`]).
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
use lash_kernel_doc::{EffectIdentity, EffectName, ErrorDatum};
use lash_kernel_vm::WaitId;

use crate::snapshot::OperationId;

/// An admitted execution the ledger keeps until it settles: its identity
/// and its call. Its request is its admission's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedEffect {
    /// Minted when its park committed: the park and its ordinal there.
    pub operation: OperationId,
    pub call: lash_sansio::ToolCallId,
}

/// How a wait stands once its park committed.
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectLedger {
    /// The waits the state stands on, by effect identity.
    pending: BTreeMap<EffectIdentity, PendingEffect>,
    /// Admitted executions the run no longer waits on (their task was
    /// cancelled) that are not known to have settled. They stay live under
    /// the run, as a join's losers do (ADR 0065), and the run's end settles
    /// each one.
    released: Vec<AdmittedEffect>,
    /// The number the next park that admits takes.
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

impl EffectLedger {
    /// The ledger of a run that has parked nowhere.
    pub fn new() -> Self {
        Self::default()
    }

    /// The ledger a stored membership names: refused when it names one
    /// wait twice, one execution twice, or an execution no park of its run
    /// admitted.
    ///
    /// # Errors
    ///
    /// [`LedgerRefusal`].
    pub fn restored(
        pending: BTreeMap<EffectIdentity, PendingEffect>,
        released: Vec<AdmittedEffect>,
        next_park: u64,
    ) -> Result<Self, LedgerRefusal> {
        let ledger = Self {
            pending,
            released,
            next_park,
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

    /// This ledger with each wait identified by `identify`: what a
    /// migration of the run's document makes of it, the waits standing as
    /// they stood.
    ///
    /// # Errors
    ///
    /// `identify`'s, for the first wait it refuses.
    pub fn identified<E>(
        &self,
        mut identify: impl FnMut(&EffectIdentity) -> Result<EffectIdentity, E>,
    ) -> Result<Self, E> {
        let mut pending = BTreeMap::new();
        for (identity, entry) in &self.pending {
            pending.insert(identify(identity)?, entry.clone());
        }
        Ok(Self {
            pending,
            released: self.released.clone(),
            next_park: self.next_park,
        })
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

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use lash_kernel_doc::{Site, TaskIdentity, Unit};

    use super::*;

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
    /// one wait twice or one execution twice, is refused when it is read.
    #[test]
    fn a_stored_ledger_that_names_what_was_never_admitted_is_refused() {
        let pending = |entries: Vec<(u64, u64, Standing)>| {
            entries
                .into_iter()
                .map(|(occurrence, wait, standing)| {
                    (identity(occurrence), self::standing(wait, standing))
                })
                .collect::<BTreeMap<_, _>>()
        };
        let read = EffectLedger::restored(
            pending(vec![(0, 0, Standing::Admitted(admitted(0, 0)))]),
            Vec::new(),
            1,
        )
        .expect("an issued ledger reads back");
        assert_eq!(read.oldest_reachable(), 0);
        assert_eq!(
            EffectLedger::restored(
                pending(vec![(0, 0, Standing::Admitted(admitted(0, 0)))]),
                Vec::new(),
                0,
            ),
            Err(LedgerRefusal::UnissuedPark { park: 0, parks: 0 })
        );
        assert_eq!(
            EffectLedger::restored(
                pending(vec![
                    (0, 0, Standing::Admitted(admitted(0, 0))),
                    (1, 0, Standing::Sleeping { until_ms: 5 }),
                ]),
                Vec::new(),
                1,
            ),
            Err(LedgerRefusal::SharedWait { wait: 0 })
        );
        assert_eq!(
            EffectLedger::restored(
                pending(vec![(0, 0, Standing::Admitted(admitted(0, 0)))]),
                vec![admitted(0, 0)],
                1,
            ),
            Err(LedgerRefusal::SharedExecution {
                operation: OperationId { run: 0, ordinal: 0 }
            })
        );
    }
}
