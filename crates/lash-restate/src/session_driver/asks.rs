//! The engine's drive asks, per session (FIG-4036): from this process, at
//! most one drive of a session is in flight on Restate and at most one is
//! queued in memory behind it.
//!
//! An ask that finds none of its session's drives in flight names a new
//! drive and sends it at once. An ask that finds one in flight joins the
//! drive queued behind it (named by the first ask to join it), which the
//! session's pump sends only once the drive in flight has ended. The
//! drive in flight admits the input at its next admission when it has one
//! left; the queued drive's first admission is the re-check that admits
//! whatever it did not.
//!
//! **No lost wake.** An ask is made after its row committed. Every ask joins
//! a drive that is sent after the ask was made. The one exception is a repeat
//! of a request already joined: that request's drive was sent after the
//! request's first ask. The ask's mutex serializes the choice between
//! "in flight" and "queued" against the pump's handover, so an ask can never
//! join a drive the pump has already sent. Restate runs a sent drive
//! exclusively on the session's object, and its first admission reads the
//! store after the send. So it sees every row whose ask joined it. A drive
//! that is about to finish when an ask arrives is therefore never trusted
//! with that ask: the ask waits for the queued drive.
//!
//! What this process holds is not durable. An ask lost with its process is
//! the ingress relay's to ask again once the row's claim lapses, exactly like
//! an ask Restate never received (ADR 0109 §3). A drive whose send failed
//! answers its asks with the failure, and the next ask of that request starts
//! over.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lash_core::SessionId;
use lash_core::engine::DriveRequestId;
use tokio::sync::watch;

use super::RestateSessionWork;

/// Ended drives whose asks a session keeps, so that a repeated ask or a late
/// waiter finds the drive it joined instead of starting another.
const ENDED_DRIVES_KEPT: usize = 8;
/// How long a session's asks are kept after its pump went idle.
const IDLE_RETENTION: Duration = Duration::from_secs(60);
/// The first pause before re-attaching to a drive whose attach failed in
/// transit, and the longest.
const ATTACH_PAUSE_FLOOR: Duration = Duration::from_millis(50);
const ATTACH_PAUSE_CEILING: Duration = Duration::from_secs(2);

/// How a drive's send went.
#[derive(Clone, Debug)]
pub(super) enum Sent {
    /// Restate accepted it under the drive's request.
    Accepted,
    /// It did not reach Restate.
    Failed(String),
}

/// One drive the engine sends for a session, named by the first ask that
/// joined it.
pub(super) struct Drive {
    request: DriveRequestId,
    sent: watch::Sender<Option<Sent>>,
}

impl Drive {
    fn new(request: DriveRequestId) -> Arc<Self> {
        Arc::new(Self {
            request,
            sent: watch::channel(None).0,
        })
    }

    /// The request this drive runs under: its send's idempotency key.
    pub(super) fn request(&self) -> &DriveRequestId {
        &self.request
    }

    /// How this drive's send went, once the pump sent it.
    pub(super) async fn sent(&self) -> Sent {
        let mut sent = self.sent.subscribe();
        // The drive holds the sender, so the wait ends only on a value.
        match sent.wait_for(Option::is_some).await {
            Ok(sent) => sent.clone().unwrap_or(Sent::Accepted),
            Err(_) => Sent::Failed(format!("drive `{}` was dropped", self.request.as_str())),
        }
    }

    fn failed(&self) -> bool {
        matches!(*self.sent.borrow(), Some(Sent::Failed(_)))
    }
}

/// The drive an ask joined.
pub(super) struct Joined {
    pub(super) drive: Arc<Drive>,
    /// Whether it waits in memory behind the session's drive in flight.
    pub(super) queued: bool,
}

/// One session's drives: the one in flight, the one queued behind it, and
/// which drive each ask joined.
#[derive(Default)]
struct SessionAsks {
    in_flight: Option<Arc<Drive>>,
    queued: Option<Arc<Drive>>,
    asks: HashMap<DriveRequestId, Arc<Drive>>,
    ended: VecDeque<Arc<Drive>>,
    /// Pumps started for the session: an idle pump forgets the session only
    /// if no pump started since.
    pumps: u64,
}

impl SessionAsks {
    /// The drive `request` joined, unless its send failed.
    fn joined(&self, request: &DriveRequestId) -> Option<&Arc<Drive>> {
        self.asks.get(request).filter(|drive| !drive.failed())
    }

    fn end(&mut self, drive: &Arc<Drive>) {
        self.ended.push_back(Arc::clone(drive));
        while self.ended.len() > ENDED_DRIVES_KEPT {
            if let Some(forgotten) = self.ended.pop_front() {
                self.asks
                    .retain(|_, joined| !Arc::ptr_eq(joined, &forgotten));
            }
        }
    }
}

/// Every session's drive asks in this engine.
#[derive(Default)]
pub(super) struct DriveAsks {
    sessions: Mutex<HashMap<SessionId, SessionAsks>>,
}

enum Handover {
    Next(Arc<Drive>),
    Idle { pumps: u64 },
}

impl DriveAsks {
    fn sessions(&self) -> MutexGuard<'_, HashMap<SessionId, SessionAsks>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The drive `request` joined, while it is kept and its send did not
    /// fail.
    pub(super) fn joined(
        &self,
        session: &SessionId,
        request: &DriveRequestId,
    ) -> Option<Arc<Drive>> {
        self.sessions()
            .get(session)?
            .joined(request)
            .map(Arc::clone)
    }

    /// Join `request` to `session`'s drive: the one it joined already, else a
    /// new one sent at once when none is in flight, else the one queued behind
    /// the drive in flight.
    pub(super) fn join(
        &self,
        engine: &RestateSessionWork,
        runtime: &tokio::runtime::Handle,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Joined {
        let mut sessions = self.sessions();
        let asks = sessions.entry(session.clone()).or_default();
        if let Some(drive) = asks.joined(&request) {
            let queued = asks
                .queued
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, drive));
            return Joined {
                drive: Arc::clone(drive),
                queued,
            };
        }
        let joined = if asks.in_flight.is_none() {
            let drive = Drive::new(request.clone());
            asks.in_flight = Some(Arc::clone(&drive));
            asks.pumps += 1;
            runtime.spawn(pump(engine.clone(), session.clone(), Arc::clone(&drive)));
            Joined {
                drive,
                queued: false,
            }
        } else {
            let drive = asks
                .queued
                .get_or_insert_with(|| Drive::new(request.clone()));
            Joined {
                drive: Arc::clone(drive),
                queued: true,
            }
        };
        asks.asks.insert(request, Arc::clone(&joined.drive));
        joined
    }

    /// `ended` ended: the queued drive goes in flight, if there is one.
    fn hand_over(&self, session: &SessionId, ended: &Arc<Drive>) -> Handover {
        let mut sessions = self.sessions();
        let asks = sessions.entry(session.clone()).or_default();
        asks.end(ended);
        asks.in_flight = asks.queued.take();
        match &asks.in_flight {
            Some(next) => Handover::Next(Arc::clone(next)),
            None => Handover::Idle { pumps: asks.pumps },
        }
    }

    /// Forget `session`'s asks if it stayed idle since its pump went idle.
    fn forget_if_idle(&self, session: &SessionId, pumps: u64) {
        let mut sessions = self.sessions();
        if sessions
            .get(session)
            .is_some_and(|asks| asks.in_flight.is_none() && asks.pumps == pumps)
        {
            sessions.remove(session);
        }
    }
}

/// Send `drive`, wait for it to end, then do the same for each drive queued
/// behind it, until none is.
async fn pump(engine: RestateSessionWork, session: SessionId, mut drive: Arc<Drive>) {
    loop {
        match engine.send_drive(&session, drive.request.clone()).await {
            Ok(_) => {
                drive.sent.send_replace(Some(Sent::Accepted));
                drive_ended(&engine, &session, &drive.request).await;
            }
            Err(error) => {
                tracing::warn!(
                    session_id = session.as_str(),
                    request = drive.request.as_str(),
                    error = %error,
                    "session drive send failed; its asks are the ingress relay's to retry"
                );
                drive
                    .sent
                    .send_replace(Some(Sent::Failed(error.to_string())));
            }
        }
        match engine.asks.hand_over(&session, &drive) {
            Handover::Next(next) => drive = next,
            Handover::Idle { pumps } => {
                tokio::time::sleep(IDLE_RETENTION).await;
                engine.asks.forget_if_idle(&session, pumps);
                return;
            }
        }
    }
}

/// Wait until the drive of `request` has ended, along its continuations. A
/// drive that answered with a failure has ended too. An attach that failed
/// in transit (the attach deadline of a long drive included) is retried.
async fn drive_ended(engine: &RestateSessionWork, session: &SessionId, request: &DriveRequestId) {
    let mut leg = request.clone();
    let mut pause = ATTACH_PAUSE_FLOOR;
    loop {
        match engine.attach_drive(session, leg.clone()).await {
            Ok(outcome) => match engine.continuation(session, &leg, &outcome) {
                Some(next) => {
                    leg = next;
                    pause = ATTACH_PAUSE_FLOOR;
                }
                None => return,
            },
            Err(crate::RestateHttpError::Request { .. }) => {
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(ATTACH_PAUSE_CEILING);
            }
            Err(_) => return,
        }
    }
}
