//! Deterministic SQLite substrate faults for external test harnesses.
//!
//! This module only exists behind the crate's `testing` feature. Production
//! factories have no injector, and production builds do not compile the hook.

use lash_sansio::sync::{LockResultExt, MutexExt};
use std::num::NonZeroU64;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

/// Transaction boundary at which one armed fault is injected.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SqliteFaultPoint {
    /// Abort immediately after `BEGIN IMMEDIATE`, before the transaction body.
    AfterBegin,
    /// Abort after the transaction body, before SQLite is asked to commit.
    BeforeCommit,
    /// Surface `SQLITE_IOERR` at the commit boundary and roll the transaction back.
    CommitIo,
}

impl SqliteFaultPoint {
    const fn index(self) -> usize {
        match self {
            Self::AfterBegin => 0,
            Self::BeforeCommit => 1,
            Self::CommitIo => 2,
        }
    }
}

/// One deterministic arm in a SQLite fault plan.
///
/// `occurrence` is one-based and counts only transactions that actually reach
/// `point` after the plan is armed. An earlier fault can therefore prevent a
/// later point from advancing until the next transaction attempt.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SqliteFaultArm {
    pub seed: u64,
    pub point: SqliteFaultPoint,
    pub occurrence: NonZeroU64,
}

impl SqliteFaultArm {
    pub const fn new(seed: u64, point: SqliteFaultPoint, occurrence: NonZeroU64) -> Self {
        Self {
            seed,
            point,
            occurrence,
        }
    }
}

/// Evidence that an armed fault reached the real SQLite transaction seam.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SqliteFaultObservation {
    /// Zero-based position of this arm in the plan installed by `arm_many`.
    pub arm_index: usize,
    pub seed: u64,
    pub point: SqliteFaultPoint,
    /// One-based occurrence of `point` reached since the plan was armed.
    pub point_occurrence: u64,
    pub write_transaction_ordinal: u64,
}

#[derive(Clone, Copy, Debug)]
struct ArmedFault {
    arm_index: usize,
    arm: SqliteFaultArm,
}

#[derive(Clone, Debug)]
struct ArmedPause {
    point: SqliteFaultPoint,
    remaining_matches: u64,
    state: Arc<PauseState>,
}

#[derive(Debug, Default)]
struct PauseState {
    state: Mutex<PauseProgress>,
    changed: Condvar,
}

#[derive(Debug, Default)]
struct PauseProgress {
    reached_ordinal: Option<u64>,
    released: bool,
}

/// One-shot deterministic pause at a real SQLite transaction boundary.
///
/// Tests use this to drop an awaiting future after tokio-rusqlite has accepted
/// its closure but before the background connection thread commits it.
#[derive(Clone, Debug)]
pub struct SqliteTransactionPause {
    state: Arc<PauseState>,
}

impl SqliteTransactionPause {
    /// Wait until the background SQLite thread reaches the armed boundary.
    pub async fn wait_until_reached(&self) -> u64 {
        let state = Arc::clone(&self.state);
        tokio::task::spawn_blocking(move || {
            let mut progress = state.state.lock_recover();
            while progress.reached_ordinal.is_none() {
                progress = state.changed.wait(progress).recover();
            }
            progress.reached_ordinal.expect("pause reached ordinal")
        })
        .await
        .expect("SQLite pause waiter task")
    }

    /// Release the background SQLite transaction to continue to commit.
    pub fn release(&self) {
        let mut progress = self.state.state.lock_recover();
        progress.released = true;
        self.state.changed.notify_all();
    }
}

#[derive(Debug, Default)]
struct InjectorState {
    armed: Vec<ArmedFault>,
    point_occurrences: [u64; 3],
    pause: Option<ArmedPause>,
    write_transaction_ordinal: u64,
    observations: Vec<SqliteFaultObservation>,
}

/// Per-factory deterministic fault controller.
///
/// Arming replaces every unconsumed arm. Each arm identifies a fault point and
/// one reached occurrence, is consumed at most once, and records its plan
/// position plus transaction ordinal for reproduction.
#[derive(Clone, Debug, Default)]
pub struct SqliteFaultInjector {
    state: Arc<Mutex<InjectorState>>,
}

impl SqliteFaultInjector {
    /// Arm one seed-selected fault point at its next reached occurrence.
    ///
    /// This preserves the original replacement behavior: any unconsumed
    /// single or multi-arm plan is discarded.
    pub fn arm(&self, seed: u64, point: SqliteFaultPoint) {
        self.arm_many([SqliteFaultArm::new(
            seed,
            point,
            NonZeroU64::new(1).expect("one is non-zero"),
        )]);
    }

    /// Replace the current plan with multiple deterministic one-shot arms.
    ///
    /// Occurrence counters start when this method is called. Observations retain
    /// each arm's original plan position; their vector order follows execution.
    pub fn arm_many(&self, arms: impl IntoIterator<Item = SqliteFaultArm>) {
        let mut state = self.lock_state();
        state.armed = arms
            .into_iter()
            .enumerate()
            .map(|(arm_index, arm)| ArmedFault { arm_index, arm })
            .collect();
        state.point_occurrences = [0; 3];
    }

    /// Return the unconsumed arms in their original plan order.
    pub fn remaining_arms(&self) -> Vec<SqliteFaultArm> {
        self.lock_state()
            .armed
            .iter()
            .map(|armed| armed.arm)
            .collect()
    }

    /// Return all injection observations recorded so far.
    pub fn observations(&self) -> Vec<SqliteFaultObservation> {
        self.lock_state().observations.clone()
    }

    /// Pause the next transaction reaching `point` until the returned handle
    /// is released.
    pub fn pause(&self, point: SqliteFaultPoint) -> SqliteTransactionPause {
        self.pause_after(point, 0)
    }

    /// Pause after `preceding_matches` earlier transactions pass the same
    /// boundary. This targets a later write in a multi-transaction operation.
    pub fn pause_after(
        &self,
        point: SqliteFaultPoint,
        preceding_matches: u64,
    ) -> SqliteTransactionPause {
        let state = Arc::new(PauseState::default());
        self.lock_state().pause = Some(ArmedPause {
            point,
            remaining_matches: preceding_matches,
            state: Arc::clone(&state),
        });
        SqliteTransactionPause { state }
    }

    pub(crate) fn begin_write(&self) -> u64 {
        let mut state = self.lock_state();
        state.write_transaction_ordinal += 1;
        state.write_transaction_ordinal
    }

    pub(crate) fn inject(
        &self,
        point: SqliteFaultPoint,
        write_transaction_ordinal: u64,
    ) -> rusqlite::Result<()> {
        let pause = {
            let mut state = self.lock_state();
            match state.pause.as_mut() {
                Some(pause) if pause.point == point && pause.remaining_matches > 0 => {
                    pause.remaining_matches -= 1;
                    None
                }
                Some(pause) if pause.point == point => state.pause.take(),
                _ => None,
            }
        };
        if let Some(pause) = pause {
            let mut progress = pause.state.state.lock_recover();
            progress.reached_ordinal = Some(write_transaction_ordinal);
            pause.state.changed.notify_all();
            while !progress.released {
                progress = pause.state.changed.wait(progress).recover();
            }
        }
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
        state.observations.push(SqliteFaultObservation {
            arm_index: armed.arm_index,
            seed: armed.arm.seed,
            point,
            point_occurrence,
            write_transaction_ordinal,
        });
        let code = match point {
            SqliteFaultPoint::AfterBegin | SqliteFaultPoint::BeforeCommit => {
                rusqlite::ffi::SQLITE_ABORT
            }
            SqliteFaultPoint::CommitIo => rusqlite::ffi::SQLITE_IOERR,
        };
        Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            Some(format!(
                "injected SQLite {point:?} fault for seed {} at write transaction {write_transaction_ordinal}",
                armed.arm.seed
            )),
        ))
    }

    fn lock_state(&self) -> MutexGuard<'_, InjectorState> {
        self.state.lock_recover()
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;

    fn arm(seed: u64, point: SqliteFaultPoint, occurrence: u64) -> SqliteFaultArm {
        SqliteFaultArm::new(
            seed,
            point,
            NonZeroU64::new(occurrence).expect("non-zero occurrence"),
        )
    }

    #[test]
    fn multiple_arms_fire_once_in_reached_point_order() {
        let injector = SqliteFaultInjector::default();
        injector.arm_many([
            arm(11, SqliteFaultPoint::AfterBegin, 1),
            arm(22, SqliteFaultPoint::CommitIo, 1),
        ]);

        let first = injector.begin_write();
        assert!(
            injector
                .inject(SqliteFaultPoint::AfterBegin, first)
                .is_err()
        );
        // The abort above prevents this transaction from reaching CommitIo.
        let second = injector.begin_write();
        injector
            .inject(SqliteFaultPoint::AfterBegin, second)
            .expect("the first arm was consumed");
        injector
            .inject(SqliteFaultPoint::BeforeCommit, second)
            .expect("no before-commit arm");
        assert!(injector.inject(SqliteFaultPoint::CommitIo, second).is_err());

        let third = injector.begin_write();
        for point in [
            SqliteFaultPoint::AfterBegin,
            SqliteFaultPoint::BeforeCommit,
            SqliteFaultPoint::CommitIo,
        ] {
            injector
                .inject(point, third)
                .expect("each arm is consumed at most once");
        }

        let observations = injector.observations();
        assert_eq!(observations.len(), 2);
        assert_eq!(observations[0].arm_index, 0);
        assert_eq!(observations[0].point, SqliteFaultPoint::AfterBegin);
        assert_eq!(observations[0].point_occurrence, 1);
        assert_eq!(observations[1].arm_index, 1);
        assert_eq!(observations[1].point, SqliteFaultPoint::CommitIo);
        assert_eq!(observations[1].point_occurrence, 1);
        assert!(injector.remaining_arms().is_empty());
    }

    #[test]
    fn arm_replaces_an_unconsumed_multi_arm_plan() {
        let injector = SqliteFaultInjector::default();
        injector.arm_many([
            arm(11, SqliteFaultPoint::AfterBegin, 1),
            arm(22, SqliteFaultPoint::CommitIo, 1),
        ]);
        injector.arm(33, SqliteFaultPoint::BeforeCommit);

        let ordinal = injector.begin_write();
        injector
            .inject(SqliteFaultPoint::AfterBegin, ordinal)
            .expect("the replaced arm must not fire");
        assert!(
            injector
                .inject(SqliteFaultPoint::BeforeCommit, ordinal)
                .is_err()
        );
        assert_eq!(
            injector.observations(),
            vec![SqliteFaultObservation {
                arm_index: 0,
                seed: 33,
                point: SqliteFaultPoint::BeforeCommit,
                point_occurrence: 1,
                write_transaction_ordinal: ordinal,
            }]
        );
    }
}
