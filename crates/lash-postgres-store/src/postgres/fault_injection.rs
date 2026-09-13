//! Deterministic PostgreSQL substrate faults for external test harnesses.
//!
//! This module only exists behind the crate's `testing` feature. Production
//! factories have no injector, and production builds do not compile the hook:
//! the `pg_sim_fault!` invocations in the write transactions expand to nothing.
//!
//! The arm and observation vocabulary is the twin of
//! `lash_sqlite_store::testing`, so one simulator scenario plan drives both
//! backends.

use lash_sansio::sync::MutexExt;
use std::num::NonZeroU64;
use std::sync::{Arc, Mutex, MutexGuard};

use lash_core::StoreError;
use serde::{Deserialize, Serialize};

/// Transaction boundary at which one armed fault is injected.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PostgresFaultPoint {
    /// Abort immediately after `BEGIN`, before the transaction body.
    AfterBegin,
    /// Abort after the transaction body, before PostgreSQL is asked to commit.
    BeforeCommit,
    /// Surface a storage failure at the commit boundary and roll the
    /// transaction back by dropping it.
    CommitIo,
}

impl PostgresFaultPoint {
    const fn index(self) -> usize {
        match self {
            Self::AfterBegin => 0,
            Self::BeforeCommit => 1,
            Self::CommitIo => 2,
        }
    }
}

/// One deterministic arm in a PostgreSQL fault plan.
///
/// `occurrence` is one-based and counts only transactions that actually reach
/// `point` after the plan is armed. An earlier fault can therefore prevent a
/// later point from advancing until the next transaction attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PostgresFaultArm {
    pub seed: u64,
    pub point: PostgresFaultPoint,
    pub occurrence: NonZeroU64,
}

impl PostgresFaultArm {
    pub const fn new(seed: u64, point: PostgresFaultPoint, occurrence: NonZeroU64) -> Self {
        Self {
            seed,
            point,
            occurrence,
        }
    }
}

/// Evidence that an armed fault reached the real PostgreSQL transaction seam.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PostgresFaultObservation {
    /// Zero-based position of this arm in the plan installed by `arm_many`.
    pub arm_index: usize,
    pub seed: u64,
    pub point: PostgresFaultPoint,
    /// One-based occurrence of `point` reached since the plan was armed.
    pub point_occurrence: u64,
    pub write_transaction_ordinal: u64,
}

#[derive(Clone, Copy, Debug)]
struct ArmedFault {
    arm_index: usize,
    arm: PostgresFaultArm,
}

#[derive(Debug, Default)]
struct InjectorState {
    armed: Vec<ArmedFault>,
    point_occurrences: [u64; 3],
    write_transaction_ordinal: u64,
    observations: Vec<PostgresFaultObservation>,
}

/// Per-factory deterministic fault controller.
///
/// Arming replaces every unconsumed arm. Each arm identifies a fault point and
/// one reached occurrence, is consumed at most once, and records its plan
/// position plus transaction ordinal for reproduction.
#[derive(Clone, Debug, Default)]
pub struct PostgresFaultInjector {
    state: Arc<Mutex<InjectorState>>,
}

impl PostgresFaultInjector {
    /// Arm one seed-selected fault point at its next reached occurrence.
    pub fn arm(&self, seed: u64, point: PostgresFaultPoint) {
        self.arm_many([PostgresFaultArm::new(
            seed,
            point,
            NonZeroU64::new(1).expect("one is non-zero"),
        )]);
    }

    /// Replace the current plan with multiple deterministic one-shot arms.
    ///
    /// Occurrence counters start when this method is called. Observations retain
    /// each arm's original plan position; their vector order follows execution.
    pub fn arm_many(&self, arms: impl IntoIterator<Item = PostgresFaultArm>) {
        let mut state = self.lock_state();
        state.armed = arms
            .into_iter()
            .enumerate()
            .map(|(arm_index, arm)| ArmedFault { arm_index, arm })
            .collect();
        state.point_occurrences = [0; 3];
    }

    /// Return the unconsumed arms in their original plan order.
    pub fn remaining_arms(&self) -> Vec<PostgresFaultArm> {
        self.lock_state()
            .armed
            .iter()
            .map(|armed| armed.arm)
            .collect()
    }

    /// Return all injection observations recorded so far.
    pub fn observations(&self) -> Vec<PostgresFaultObservation> {
        self.lock_state().observations.clone()
    }

    /// Stamp the next write transaction and return its ordinal.
    pub fn begin_write(&self) -> u64 {
        let mut state = self.lock_state();
        state.write_transaction_ordinal += 1;
        state.write_transaction_ordinal
    }

    /// Consume the arm matching `point` at its reached occurrence, if any.
    pub fn inject(
        &self,
        point: PostgresFaultPoint,
        write_transaction_ordinal: u64,
    ) -> Result<(), StoreError> {
        let mut state = self.lock_state();
        let point_occurrence = {
            let occurrence = &mut state.point_occurrences[point.index()];
            *occurrence += 1;
            *occurrence
        };
        let Some(position) = state.armed.iter().position(|armed| {
            armed.arm.point == point && armed.arm.occurrence.get() == point_occurrence
        }) else {
            return Ok(());
        };
        let armed = state.armed.remove(position);
        state.observations.push(PostgresFaultObservation {
            arm_index: armed.arm_index,
            seed: armed.arm.seed,
            point,
            point_occurrence,
            write_transaction_ordinal,
        });
        Err(StoreError::StorageFailure {
            backend: "postgres",
            message: format!(
                "injected PostgreSQL {point:?} fault for seed {} at write transaction {write_transaction_ordinal}",
                armed.arm.seed
            ),
        })
    }

    fn lock_state(&self) -> MutexGuard<'_, InjectorState> {
        self.state.lock_recover()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arm(seed: u64, point: PostgresFaultPoint, occurrence: u64) -> PostgresFaultArm {
        PostgresFaultArm::new(
            seed,
            point,
            NonZeroU64::new(occurrence).expect("non-zero occurrence"),
        )
    }

    #[test]
    fn multiple_arms_fire_once_in_reached_point_order() {
        let injector = PostgresFaultInjector::default();
        injector.arm_many([
            arm(11, PostgresFaultPoint::AfterBegin, 1),
            arm(22, PostgresFaultPoint::CommitIo, 1),
        ]);

        let first = injector.begin_write();
        assert!(
            injector
                .inject(PostgresFaultPoint::AfterBegin, first)
                .is_err()
        );
        // The abort above prevents this transaction from reaching CommitIo.
        let second = injector.begin_write();
        injector
            .inject(PostgresFaultPoint::AfterBegin, second)
            .expect("the first arm was consumed");
        injector
            .inject(PostgresFaultPoint::BeforeCommit, second)
            .expect("no before-commit arm");
        assert!(
            injector
                .inject(PostgresFaultPoint::CommitIo, second)
                .is_err()
        );

        let third = injector.begin_write();
        for point in [
            PostgresFaultPoint::AfterBegin,
            PostgresFaultPoint::BeforeCommit,
            PostgresFaultPoint::CommitIo,
        ] {
            injector
                .inject(point, third)
                .expect("each arm is consumed at most once");
        }

        let observations = injector.observations();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].arm_index, 0);
        assert_eq!(observations[0].point, PostgresFaultPoint::AfterBegin);
        assert_eq!(observations[0].point_occurrence, 1);
        assert_eq!(observations[1].arm_index, 1);
        assert_eq!(observations[1].point, PostgresFaultPoint::CommitIo);
        assert_eq!(observations[1].point_occurrence, 1);
        assert!(injector.remaining_arms().is_empty());
    }

    #[test]
    fn arm_replaces_an_unconsumed_multi_arm_plan() {
        let injector = PostgresFaultInjector::default();
        injector.arm_many([
            arm(11, PostgresFaultPoint::AfterBegin, 1),
            arm(22, PostgresFaultPoint::CommitIo, 1),
        ]);
        injector.arm(33, PostgresFaultPoint::BeforeCommit);

        let ordinal = injector.begin_write();
        injector
            .inject(PostgresFaultPoint::AfterBegin, ordinal)
            .expect("the replaced arm must not fire");
        let error = injector
            .inject(PostgresFaultPoint::BeforeCommit, ordinal)
            .expect_err("the replacing arm fires");
        assert!(matches!(error, StoreError::StorageFailure { .. }));
        assert_eq!(
            injector.observations(),
            vec![PostgresFaultObservation {
                arm_index: 0,
                seed: 33,
                point: PostgresFaultPoint::BeforeCommit,
                point_occurrence: 1,
                write_transaction_ordinal: ordinal,
            }]
        );
    }
}
