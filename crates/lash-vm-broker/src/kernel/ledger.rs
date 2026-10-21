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
use lash_kernel_doc::{EffectId, ErrorDatum};
use lash_kernel_vm::WaitId;
use lash_vocabulary::names::OperationUse;

use crate::snapshot::OperationId;

/// An admitted execution the ledger keeps until it settles: its identity
/// and its call. Its request is its admission's.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdmittedEffect {
    /// Minted when its park committed: the park and its ordinal there.
    pub operation: OperationId,
    pub call: lash_protocol_types::ToolCallId,
}

/// How a wait stands once its park committed.
#[derive(Clone, Debug, PartialEq)]
pub enum Standing {
    /// An admitted execution, run through the admitted-execution lifecycle.
    Admitted(AdmittedEffect),
    /// The parent refused the effect: it is answered with this error and
    /// nothing was dispatched.
    Refused(ErrorDatum),
}

/// One wait the saved state stands on.
#[derive(Clone, Debug, PartialEq)]
pub struct PendingEffect {
    /// The machine's number for the wait, which its outcome is delivered
    /// to.
    pub wait: lash_protocol_types::WaitId,
    /// The effect performed; none for a sleep.
    pub effect: Option<OperationUse>,
    pub standing: Standing,
}

impl PendingEffect {
    pub fn wait(&self) -> WaitId {
        self.wait
    }
}

/// What a run's parks appended over its whole life, across every
/// activation and resume (FIG-6788): the effects and sleeps they admitted,
/// and the page bytes the run sealed into the checkpoints they saved. It
/// commits with each save, so a resumed run counts on from its last one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Growth {
    pub effects: u64,
    pub checkpoint_bytes: u64,
}

/// The broker's state that commits with a run's saved state.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EffectLedger {
    /// The waits the state stands on, by effect identity.
    pending: BTreeMap<EffectId, PendingEffect>,
    /// Admitted executions the run no longer waits on (their task was
    /// cancelled) that are not known to have settled. They stay live under
    /// the run, as a join's losers do (ADR 0065), and the run's end settles
    /// each one.
    released: Vec<AdmittedEffect>,
    /// The number the next park that admits takes.
    next_park: u64,
    /// What the run's saves appended so far.
    growth: Growth,
}

/// A stored ledger that names what no park of its run admitted.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LedgerRefusal {
    #[error("the ledger names an execution of park {park}, and its run has admitted {parks}")]
    UnissuedPark { park: u64, parks: u64 },
    #[error("two of the ledger's waits share wait {wait}")]
    SharedWait { wait: lash_protocol_types::WaitId },
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
        pending: BTreeMap<EffectId, PendingEffect>,
        released: Vec<AdmittedEffect>,
        next_park: u64,
        growth: Growth,
    ) -> Result<Self, LedgerRefusal> {
        let ledger = Self {
            pending,
            released,
            next_park,
            growth,
        };
        let mut waits: Vec<WaitId> = ledger.pending.values().map(|entry| entry.wait).collect();
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
        mut identify: impl FnMut(&EffectId) -> Result<EffectId, E>,
    ) -> Result<Self, E> {
        let mut pending = BTreeMap::new();
        for (identity, entry) in &self.pending {
            pending.insert(identify(identity)?, entry.clone());
        }
        Ok(Self {
            pending,
            released: self.released.clone(),
            next_park: self.next_park,
            growth: self.growth,
        })
    }

    /// The waits the state stands on, in identity order.
    pub fn pending(&self) -> impl Iterator<Item = (&EffectId, &PendingEffect)> {
        self.pending.iter()
    }

    pub fn get(&self, identity: &EffectId) -> Option<&PendingEffect> {
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

    /// What the run's saves appended so far.
    pub fn growth(&self) -> Growth {
        self.growth
    }

    /// The run's growth as the save this ledger commits with leaves it.
    pub(crate) fn grown(&mut self, growth: Growth) {
        self.growth = growth;
    }

    /// Every admitted execution the ledger names, awaited or released.
    pub fn executions(&self) -> impl Iterator<Item = &AdmittedEffect> {
        self.pending
            .values()
            .filter_map(|entry| match &entry.standing {
                Standing::Admitted(admitted) => Some(admitted),
                Standing::Refused(_) => None,
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
    pub(crate) fn stand(&mut self, identity: EffectId, entry: PendingEffect) -> bool {
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
    pub fn consume(&mut self, identity: &EffectId) -> Option<PendingEffect> {
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
            .find(|(_, entry)| entry.wait == wait)
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
