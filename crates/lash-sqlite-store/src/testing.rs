//! Deterministic SQLite substrate faults for external test harnesses.
//!
//! This module only exists behind the crate's `testing` feature. Production
//! factories have no injector, and production builds do not compile the hook.

use std::sync::{Arc, Mutex, MutexGuard};

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

/// Evidence that an armed fault reached the real SQLite transaction seam.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SqliteFaultObservation {
    pub seed: u64,
    pub point: SqliteFaultPoint,
    pub write_transaction_ordinal: u64,
}

#[derive(Clone, Copy, Debug)]
struct ArmedFault {
    seed: u64,
    point: SqliteFaultPoint,
}

#[derive(Debug, Default)]
struct InjectorState {
    armed: Option<ArmedFault>,
    write_transaction_ordinal: u64,
    observations: Vec<SqliteFaultObservation>,
}

/// Per-factory, one-shot deterministic fault controller.
///
/// Arming replaces any unconsumed fault. A matching transaction consumes the
/// fault exactly once and records its transaction ordinal for reproduction.
#[derive(Clone, Debug, Default)]
pub struct SqliteFaultInjector {
    state: Arc<Mutex<InjectorState>>,
}

impl SqliteFaultInjector {
    /// Arm one seed-selected fault point.
    pub fn arm(&self, seed: u64, point: SqliteFaultPoint) {
        self.lock_state().armed = Some(ArmedFault { seed, point });
    }

    /// Return all injection observations recorded so far.
    pub fn observations(&self) -> Vec<SqliteFaultObservation> {
        self.lock_state().observations.clone()
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
        let mut state = self.lock_state();
        let Some(armed) = state.armed else {
            return Ok(());
        };
        if armed.point != point {
            return Ok(());
        }
        state.armed = None;
        state.observations.push(SqliteFaultObservation {
            seed: armed.seed,
            point,
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
                armed.seed
            )),
        ))
    }

    fn lock_state(&self) -> MutexGuard<'_, InjectorState> {
        self.state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}
