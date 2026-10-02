//! The engine's shift asks, per session (FIG-4036): from this process, at
//! most one shift of a session is in flight on Restate and at most one is
//! queued in memory behind it.
//!
//! An ask that finds none of its session's shifts in flight names a new
//! shift and sends it at once. An ask that finds one in flight joins the
//! shift queued behind it (named by the first ask to join it), which the
//! session's pump sends only once the shift in flight has ended. The
//! shift in flight admits the input at its next admission when it has one
//! left; the queued shift's first admission is the re-check that admits
//! whatever it did not.
//!
//! **No lost wake.** An ask is made after its row committed. Every ask joins
//! a shift that is sent after the ask was made. The one exception is a repeat
//! of a request already joined: that request's shift was sent after the
//! request's first ask. The ask's mutex serializes the choice between
//! "in flight" and "queued" against the pump's handover, so an ask can never
//! join a shift the pump has already sent. Restate runs a sent shift
//! exclusively on the session's object, and its first admission reads the
//! store after the send. So it sees every row whose ask joined it. A shift
//! that is about to finish when an ask arrives is therefore never trusted
//! with that ask: the ask waits for the queued shift.
//!
//! What this process holds is not durable. An ask lost with its process is
//! the ingress relay's to ask again once the row's claim lapses, exactly like
//! an ask Restate never received (ADR 0109 §3). A shift whose send failed
//! answers its asks with the failure, and the next ask of that request starts
//! over.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use lash_core::SessionId;
use lash_core::engine::ShiftRequestId;
use tokio::sync::watch;

use super::RestateSessionWork;

/// Ended executes whose asks a session keeps, so that a repeated ask or a late
/// waiter finds the shift it joined instead of starting another.
const ENDED_SHIFTS_KEPT: usize = 8;
/// How long a session's asks are kept after its pump went idle.
const IDLE_RETENTION: Duration = Duration::from_secs(60);
/// The first pause before re-attaching to a shift whose attach failed in
/// transit, and the longest.
const ATTACH_PAUSE_FLOOR: Duration = Duration::from_millis(50);
const ATTACH_PAUSE_CEILING: Duration = Duration::from_secs(2);

/// How a shift's send went.
#[derive(Clone, Debug)]
pub(super) enum Sent {
    /// Restate accepted it under the shift's request.
    Accepted,
    /// It did not reach Restate, or Restate refused it.
    Failed(lash_core::engine::EngineRefusal),
}

/// One shift the engine sends for a session, named by the first ask that
/// joined it.
pub(super) struct Shift {
    request: ShiftRequestId,
    sent: watch::Sender<Option<Sent>>,
}

impl Shift {
    fn new(request: ShiftRequestId) -> Arc<Self> {
        Arc::new(Self {
            request,
            sent: watch::channel(None).0,
        })
    }

    /// The request this shift runs under: its send's idempotency key.
    pub(super) fn request(&self) -> &ShiftRequestId {
        &self.request
    }

    /// How this shift's send went, once the pump sent it.
    pub(super) async fn sent(&self) -> Sent {
        let mut sent = self.sent.subscribe();
        // The shift holds the sender, so the wait ends only on a value.
        match sent.wait_for(Option::is_some).await {
            Ok(sent) => sent.clone().unwrap_or(Sent::Accepted),
            Err(_) => Sent::Failed(lash_core::engine::EngineRefusal::retryable(
                lash_core::RuntimeErrorCode::EngineControlRequest,
                format!("shift `{}` was dropped", self.request.as_str()),
            )),
        }
    }

    fn failed(&self) -> bool {
        matches!(*self.sent.borrow(), Some(Sent::Failed(_)))
    }
}

/// The shift an ask joined.
pub(super) struct Joined {
    pub(super) shift: Arc<Shift>,
    /// Whether it waits in memory behind the session's shift in flight.
    pub(super) queued: bool,
}

/// One session's shifts: the one in flight, the one queued behind it, and
/// which shift each ask joined.
#[derive(Default)]
struct SessionAsks {
    in_flight: Option<Arc<Shift>>,
    queued: Option<Arc<Shift>>,
    asks: HashMap<ShiftRequestId, Arc<Shift>>,
    ended: VecDeque<Arc<Shift>>,
    /// Pumps started for the session: an idle pump forgets the session only
    /// if no pump started since.
    pumps: u64,
}

impl SessionAsks {
    /// The shift `request` joined, unless its send failed.
    fn joined(&self, request: &ShiftRequestId) -> Option<&Arc<Shift>> {
        self.asks.get(request).filter(|shift| !shift.failed())
    }

    fn end(&mut self, shift: &Arc<Shift>) {
        self.ended.push_back(Arc::clone(shift));
        while self.ended.len() > ENDED_SHIFTS_KEPT {
            if let Some(forgotten) = self.ended.pop_front() {
                self.asks
                    .retain(|_, joined| !Arc::ptr_eq(joined, &forgotten));
            }
        }
    }
}

/// Every session's shift asks in this engine.
#[derive(Default)]
pub(super) struct ShiftAsks {
    sessions: Mutex<HashMap<SessionId, SessionAsks>>,
}

enum Handover {
    Next(Arc<Shift>),
    Idle { pumps: u64 },
}

impl ShiftAsks {
    fn sessions(&self) -> MutexGuard<'_, HashMap<SessionId, SessionAsks>> {
        self.sessions.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The shift `request` joined, while it is kept and its send did not
    /// fail.
    pub(super) fn joined(
        &self,
        session: &SessionId,
        request: &ShiftRequestId,
    ) -> Option<Arc<Shift>> {
        self.sessions()
            .get(session)?
            .joined(request)
            .map(Arc::clone)
    }

    /// Join `request` to `session`'s shift: the one it joined already, else a
    /// new one sent at once when none is in flight, else the one queued behind
    /// the shift in flight.
    pub(super) fn join(
        &self,
        engine: &RestateSessionWork,
        runtime: &tokio::runtime::Handle,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Joined {
        let mut sessions = self.sessions();
        let asks = sessions.entry(session.clone()).or_default();
        if let Some(shift) = asks.joined(&request) {
            let queued = asks
                .queued
                .as_ref()
                .is_some_and(|queued| Arc::ptr_eq(queued, shift));
            return Joined {
                shift: Arc::clone(shift),
                queued,
            };
        }
        let joined = if asks.in_flight.is_none() {
            let shift = Shift::new(request.clone());
            asks.in_flight = Some(Arc::clone(&shift));
            asks.pumps += 1;
            runtime.spawn(pump(engine.clone(), session.clone(), Arc::clone(&shift)));
            Joined {
                shift,
                queued: false,
            }
        } else {
            let shift = asks
                .queued
                .get_or_insert_with(|| Shift::new(request.clone()));
            Joined {
                shift: Arc::clone(shift),
                queued: true,
            }
        };
        asks.asks.insert(request, Arc::clone(&joined.shift));
        joined
    }

    /// `ended` ended: the queued shift goes in flight, if there is one.
    fn hand_over(&self, session: &SessionId, ended: &Arc<Shift>) -> Handover {
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

/// Send `shift`, wait for it to end, then do the same for each shift queued
/// behind it, until none is.
async fn pump(engine: RestateSessionWork, session: SessionId, mut shift: Arc<Shift>) {
    loop {
        match engine.send_shift(&session, shift.request.clone()).await {
            Ok(_) => {
                shift.sent.send_replace(Some(Sent::Accepted));
                shift_ended(&engine, &session, &shift.request).await;
            }
            Err(error) => {
                tracing::warn!(
                    session_id = session.as_str(),
                    request = shift.request.as_str(),
                    error = %error,
                    "session shift send failed; its asks are the ingress relay's to retry"
                );
                shift
                    .sent
                    .send_replace(Some(Sent::Failed(crate::session_control::refusal(error))));
            }
        }
        match engine.asks.hand_over(&session, &shift) {
            Handover::Next(next) => shift = next,
            Handover::Idle { pumps } => {
                tokio::time::sleep(IDLE_RETENTION).await;
                engine.asks.forget_if_idle(&session, pumps);
                return;
            }
        }
    }
}

/// Wait until the shift of `request` has ended, along its continuations. A
/// shift that answered with a failure has ended too. An attach that failed
/// in transit (the attach deadline of a long shift included) is retried.
async fn shift_ended(engine: &RestateSessionWork, session: &SessionId, request: &ShiftRequestId) {
    let mut leg = lash_core::engine::ShiftRequest {
        session: session.clone(),
        request: request.clone(),
        intended_lane: None,
    };
    let mut pause = ATTACH_PAUSE_FLOOR;
    loop {
        match engine.attach_drive_request(&leg).await {
            Ok(outcome) => match engine.continuation(&leg, &outcome) {
                Some(next) => {
                    leg = next;
                    pause = ATTACH_PAUSE_FLOOR;
                }
                None => return,
            },
            Err(super::SendShiftError::Http(error))
                if error.classification() == crate::RestateHttpErrorClass::Transient =>
            {
                tokio::time::sleep(pause).await;
                pause = (pause * 2).min(ATTACH_PAUSE_CEILING);
            }
            Err(_) => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_http_transport::{
        HttpRequest, HttpResponse, HttpResponseBody, HttpTransport, LlmTransportError,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug, Default)]
    struct RestartingIngress {
        calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl HttpTransport for RestartingIngress {
        async fn send(
            &self,
            _request: HttpRequest,
            _timeout: Option<Duration>,
        ) -> Result<HttpResponse, LlmTransportError> {
            let restarting = self.calls.fetch_add(1, Ordering::SeqCst) == 0;
            Ok(HttpResponse {
                status: if restarting { 503 } else { 200 },
                headers: Vec::new(),
                body: if restarting {
                    HttpResponseBody::buffered(
                        r#"{"source":"ingress","message":"node restarting"}"#,
                    )
                } else {
                    HttpResponseBody::buffered(
                        serde_json::to_vec(&crate::Reply::at(
                            crate::RESTATE_WIRE_VERSION,
                            lash_core::engine::ShiftOutcome {
                                ran: Vec::new(),
                                stop: lash_core::engine::ShiftStop::Idle,
                            },
                        ))
                        .expect("encode the shift's real outcome"),
                    )
                },
            })
        }
    }

    #[tokio::test]
    async fn a_shift_observer_waits_for_the_real_end_after_ingress_unavailability() {
        let ingress = Arc::new(RestartingIngress::default());
        let backend = crate::tests::memory_engine().await;
        let mut engine = backend.session_work_engine().as_ref().clone();
        engine.ingress = crate::RestateIngressClient::new(
            crate::RestateConnection::with_transport("https://restate.invalid", ingress.clone()),
        );

        tokio::time::timeout(
            Duration::from_secs(2),
            shift_ended(
                &engine,
                &SessionId::from("restarting-shift"),
                &ShiftRequestId::new("shift-1"),
            ),
        )
        .await
        .expect("the real outcome follows one availability fault");
        assert_eq!(
            ingress.calls.load(Ordering::SeqCst),
            2,
            "an unavailable ingress cannot count as the end of a durable shift"
        );
    }
}
