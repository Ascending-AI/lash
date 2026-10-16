//! Node lease renewal and observations on the runner's monotonic clock.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use lash_core_ids::clock::Clock;
use tokio::sync::watch;

use super::{Liveness, Stopped};
use crate::config::LeaseSettings;
use crate::error::DurableError;
use crate::port::{DurableStore, HeartbeatOutcome, NodeLease};

#[derive(Default)]
struct Registration {
    lease: Option<NodeLease>,
    last_renewed: Option<Instant>,
    serving_interval: Option<(Instant, Instant)>,
}

/// One runner's registered boot and serving interval, observed without querying the store.
#[derive(Clone, Default)]
pub struct NodeRegistration(Arc<Mutex<Registration>>);

impl NodeRegistration {
    pub fn snapshot(&self) -> Option<NodeLease> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lease
            .clone()
    }

    /// Last successful renewal's send instant and the serving-stop decision,
    /// on the runner's injected monotonic clock. Shutdown is excluded.
    /// Registration is the initial renewal when no heartbeat has succeeded.
    #[must_use]
    pub fn serving_interval(&self) -> Option<(Instant, Instant)> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .serving_interval
    }

    pub(super) fn registered(&self, lease: NodeLease, sent: Instant) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Registration {
            lease: Some(lease),
            last_renewed: Some(sent),
            serving_interval: None,
        };
    }

    fn renewed(&self, sent: Instant) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_renewed = Some(sent);
    }

    pub(super) fn stopped(&self, at: Instant) {
        let mut registration = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        registration.serving_interval = registration.last_renewed.map(|renewed| (renewed, at));
    }
}

/// Renew one heartbeat at a time. Each successful renewal moves the deadline
/// to `self_stop_after` past its send instant, never its answer. The deadline
/// races both the next send and an unanswered heartbeat, and `ended` tells the
/// serving loop when renewal stops.
pub(super) struct Renewal {
    pub store: Arc<dyn DurableStore>,
    pub clock: Arc<dyn Clock>,
    pub lease: NodeLease,
    pub settings: LeaseSettings,
    pub liveness: Liveness,
    pub registration: NodeRegistration,
    pub registering: Instant,
    pub ended: watch::Sender<Option<Stopped>>,
}

impl Renewal {
    pub async fn run(self) {
        let Self {
            store,
            clock,
            lease,
            settings,
            liveness,
            registration,
            registering,
            ended,
        } = self;
        let mut deadline = registering + settings.self_stop_after;
        let mut next = registering + settings.heartbeat_every;
        let stopped = loop {
            tokio::select! {
                biased;
                () = clock.sleep_until(deadline) => break Stopped::Unrenewed,
                () = clock.sleep_until(next) => {}
            }
            let sent = clock.now();
            next = sent + settings.heartbeat_every;
            tokio::select! {
                biased;
                () = clock.sleep_until(deadline) => break Stopped::Unrenewed,
                beat = store.heartbeat(&lease) => match beat {
                    Ok(HeartbeatOutcome::Renewed { .. }) => {
                        deadline = deadline.max(sent + settings.self_stop_after);
                        liveness.renew(deadline);
                        registration.renewed(sent);
                    }
                    Ok(HeartbeatOutcome::Reaped) | Err(DurableError::NodeLeaseLost { .. }) => {
                        break Stopped::LeaseLost;
                    }
                    // Retried at the next heartbeat.
                    Err(_) => {}
                },
            }
        };
        if stopped == Stopped::LeaseLost {
            liveness.lose();
        }
        ended.send_replace(Some(stopped));
    }
}
