//! Per-session drive counters of the in-process engine: what
//! [`SessionWorkEngine::await_drive`](crate::SessionWorkEngine::await_drive)
//! waits on (FIG-3600 S5b).
//!
//! Every ask raises the session's `requested` count. Each run of the
//! dispatcher records, when it ends, the `requested` count it saw when its
//! last attempt began and how the run ended. A waiter for ask `n` is answered
//! by the first run that began after ask `n` was counted: that run's drive
//! admitted whatever the ask followed.

use std::collections::HashMap;
use std::sync::Mutex;

use lash_sansio::sync::MutexExt;

use crate::engine::{DriveAbort, DriveOutcome, DriveStop};
use crate::{RuntimeError, SessionId};

/// How one dispatcher run ended, as a waiter learns it.
#[derive(Clone, Debug)]
pub(super) enum RunEnd {
    /// The run drove the session to a stop.
    Drove,
    /// The run gave up on a fault a later run may not meet.
    Retry(RuntimeError),
    /// The run was refused, or the engine shut down.
    Refused(RuntimeError),
}

impl RunEnd {
    fn answer(self) -> Result<DriveOutcome, DriveAbort> {
        match self {
            // The in-process engine reports that a drive ran after the ask,
            // not what it ran: a waiter reads the outcome from the store.
            Self::Drove => Ok(DriveOutcome {
                ran: Vec::new(),
                stop: DriveStop::Idle,
            }),
            Self::Retry(error) => Err(DriveAbort::Retry(error)),
            Self::Refused(error) => Err(DriveAbort::Refused(error)),
        }
    }
}

#[derive(Default)]
struct SessionDrives {
    requested: u64,
    completed: u64,
    last: Option<RunEnd>,
}

#[derive(Default)]
pub(crate) struct DriveLedger {
    sessions: Mutex<HashMap<SessionId, SessionDrives>>,
    changed: tokio::sync::Notify,
}

impl DriveLedger {
    /// Count one ask for `session`; answers the ask's number.
    pub(super) fn request(&self, session: &SessionId) -> u64 {
        let mut sessions = self.sessions.lock_recover();
        let drives = sessions.entry(session.clone()).or_default();
        drives.requested = drives.requested.saturating_add(1);
        drives.requested
    }

    /// The asks counted so far: what a run beginning now answers.
    pub(super) fn requested(&self, session: &SessionId) -> u64 {
        self.sessions
            .lock_recover()
            .get(session)
            .map_or(0, |drives| drives.requested)
    }

    /// A run that began having seen `upto` asks ended with `end`.
    pub(super) fn complete(&self, session: &SessionId, upto: u64, end: RunEnd) {
        {
            let mut sessions = self.sessions.lock_recover();
            let drives = sessions.entry(session.clone()).or_default();
            if upto >= drives.completed {
                drives.completed = upto;
                drives.last = Some(end);
            }
        }
        self.changed.notify_waiters();
    }

    /// Wait until a run that began after ask `ask` ended; answer how.
    pub(super) async fn wait(
        &self,
        session: &SessionId,
        ask: u64,
    ) -> Result<DriveOutcome, DriveAbort> {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let sessions = self.sessions.lock_recover();
                if let Some(drives) = sessions.get(session)
                    && drives.completed >= ask
                    && let Some(end) = drives.last.clone()
                {
                    return end.answer();
                }
            }
            notified.await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_waiter_is_answered_by_the_first_run_that_began_after_its_ask() {
        let ledger = std::sync::Arc::new(DriveLedger::default());
        let session = SessionId::from("s");
        let before = ledger.requested(&session);
        let ask = ledger.request(&session);
        let waiter = {
            let ledger = std::sync::Arc::clone(&ledger);
            let session = session.clone();
            crate::task::spawn(async move { ledger.wait(&session, ask).await })
        };
        // A run that began before the ask does not answer it.
        ledger.complete(&session, before, RunEnd::Drove);
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        let upto = ledger.requested(&session);
        ledger.complete(
            &session,
            upto,
            RunEnd::Refused(RuntimeError::new(
                crate::RuntimeErrorCode::QueuedWork,
                "refused",
            )),
        );
        let answer = waiter.await.expect("the waiter ends");
        assert!(matches!(answer, Err(DriveAbort::Refused(_))));
    }
}
