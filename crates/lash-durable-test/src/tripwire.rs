//! [`Tripwire`]: the replay tripwire (ADR 0132 §2, laws NR-1 to NR-4).
//!
//! A [`DurableProbe`] that counts, per identity, everything hidden replay
//! would make the runtime do: body entries per admitted execution, VM
//! program entries (a fresh run from instruction 0) per execution, outcome
//! lookups made on behalf of re-running code, emissions of an
//! already-committed `RunLedger` ordinal, and turn restores from a
//! checkpoint. A law reads the counts after resume and fails on any above
//! its bound.

use lash_durable::DurableProbe;
use lash_durable::domain::{AdmittedId, ExecKey, Ordinal, OwnerKey, RunSeq};
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The counts one [`Tripwire`] holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TripwireCounts {
    /// Body entries per admitted execution.
    pub bodies: BTreeMap<AdmittedId, usize>,
    /// Fresh VM program entries per execution.
    pub vm_programs: BTreeMap<ExecKey, usize>,
    /// Outcome lookups on behalf of re-running code, per admitted
    /// execution.
    pub outcome_lookups: BTreeMap<AdmittedId, usize>,
    /// Emissions of an already-committed ordinal, per `(owner, run,
    /// ordinal)`.
    pub committed_ordinals: BTreeMap<(OwnerKey, RunSeq, Ordinal), usize>,
    /// Restores from a checkpoint, per `(session, run)`.
    pub restores: BTreeMap<(SessionId, TurnId), usize>,
}

/// A counting [`DurableProbe`].
#[derive(Debug, Default)]
pub struct Tripwire {
    counts: Mutex<TripwireCounts>,
}

fn bump<K: Ord>(map: &mut BTreeMap<K, usize>, key: K) {
    *map.entry(key).or_default() += 1;
}

impl Tripwire {
    /// A tripwire with nothing counted.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything counted so far.
    #[must_use]
    pub fn counts(&self) -> TripwireCounts {
        self.counts.lock_recover().clone()
    }

    /// How often `id`'s body was entered.
    #[must_use]
    pub fn bodies(&self, id: &AdmittedId) -> usize {
        self.counts
            .lock_recover()
            .bodies
            .get(id)
            .copied()
            .unwrap_or(0)
    }

    /// How often `exec`'s program was entered fresh.
    #[must_use]
    pub fn vm_programs(&self, exec: &ExecKey) -> usize {
        self.counts
            .lock_recover()
            .vm_programs
            .get(exec)
            .copied()
            .unwrap_or(0)
    }

    /// Outcome lookups on behalf of re-running code, in total.
    #[must_use]
    pub fn outcome_lookups(&self) -> usize {
        self.counts.lock_recover().outcome_lookups.values().sum()
    }

    /// Emissions of an already-committed ordinal, in total.
    #[must_use]
    pub fn committed_ordinals(&self) -> usize {
        self.counts.lock_recover().committed_ordinals.values().sum()
    }

    /// How often `(session, run)` was restored from its checkpoint.
    #[must_use]
    pub fn restores(&self, session: &SessionId, run: &TurnId) -> usize {
        self.counts
            .lock_recover()
            .restores
            .get(&(session.clone(), run.clone()))
            .copied()
            .unwrap_or(0)
    }

    /// Forget everything counted, e.g. at a resume.
    pub fn reset(&self) {
        *self.counts.lock_recover() = TripwireCounts::default();
    }
}

impl DurableProbe for Tripwire {
    fn body_entered(&self, id: &AdmittedId) {
        bump(&mut self.counts.lock_recover().bodies, id.clone());
    }

    fn vm_program_entered(&self, exec: &ExecKey) {
        bump(&mut self.counts.lock_recover().vm_programs, exec.clone());
    }

    fn outcome_lookup(&self, id: &AdmittedId) {
        bump(&mut self.counts.lock_recover().outcome_lookups, id.clone());
    }

    fn committed_ordinal_emitted(&self, owner: &OwnerKey, run: RunSeq, ordinal: Ordinal) {
        bump(
            &mut self.counts.lock_recover().committed_ordinals,
            (owner.clone(), run, ordinal),
        );
    }

    fn checkpoint_restored(&self, session: &SessionId, run: &TurnId) {
        bump(
            &mut self.counts.lock_recover().restores,
            (session.clone(), run.clone()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each kind of event counts under its own identity, and nothing else.
    #[test]
    fn the_tripwire_counts_each_event_kind_per_identity() {
        let wire = Tripwire::new();
        let session = SessionId::try_from("s".to_owned()).unwrap();
        let run = TurnId::try_from("r".to_owned()).unwrap();
        let owner = OwnerKey::Turn(session.clone(), run.clone());
        let id = |ordinal| AdmittedId {
            owner: owner.clone(),
            run: RunSeq(0),
            ordinal: Ordinal(ordinal),
        };
        let exec = ExecKey::Cell(
            session.clone(),
            run.clone(),
            lash_durable::domain::CellId::new("c"),
        );

        wire.body_entered(&id(1));
        wire.body_entered(&id(1));
        wire.body_entered(&id(2));
        wire.vm_program_entered(&exec);
        wire.outcome_lookup(&id(1));
        wire.committed_ordinal_emitted(&owner, RunSeq(0), Ordinal(1));
        wire.committed_ordinal_emitted(&owner, RunSeq(0), Ordinal(1));
        wire.checkpoint_restored(&session, &run);

        assert_eq!(wire.bodies(&id(1)), 2);
        assert_eq!(wire.bodies(&id(2)), 1);
        assert_eq!(wire.bodies(&id(3)), 0);
        assert_eq!(wire.vm_programs(&exec), 1);
        assert_eq!(wire.outcome_lookups(), 1);
        assert_eq!(wire.committed_ordinals(), 2);
        assert_eq!(wire.restores(&session, &run), 1);
        wire.reset();
        assert_eq!(wire.counts(), TripwireCounts::default());
    }
}
