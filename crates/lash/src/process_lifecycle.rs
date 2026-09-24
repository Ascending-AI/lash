//! Project journaled process transitions into live session observation.
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

use lash_core::facade_support::{ProcessEventSink, ProcessWorkerFault, RuntimeHandle};
use lash_core::{
    LiveReplayEventDraft, LiveReplayStore, ProcessEvent, ProcessRegistry,
    SessionObservationEventPayload, SessionProcessEventKind,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{ProcessId, SessionId};

type SessionPublisher = dyn Fn(SessionProcessEventKind, ProcessId) -> bool + Send + Sync;

pub(crate) struct ProcessLifecycleRoute {
    feed: Weak<ProcessLifecycleFeed>,
    session_id: SessionId,
    publisher: Arc<SessionPublisher>,
}

impl Drop for ProcessLifecycleRoute {
    fn drop(&mut self) {
        if let Some(feed) = self.feed.upgrade() {
            let mut routes = feed.routes.lock_recover();
            if let Some(publishers) = routes.get_mut(&self.session_id) {
                publishers.retain(|publisher| !Arc::ptr_eq(publisher, &self.publisher));
                if publishers.is_empty() {
                    routes.remove(&self.session_id);
                }
            }
        }
    }
}

pub(crate) struct ProcessLifecycleFeed {
    registry: OnceLock<Arc<dyn ProcessRegistry>>,
    /// Every open handle of a session registers a publisher; one live
    /// publisher per session publishes each transition exactly once.
    routes: Mutex<HashMap<SessionId, Vec<Arc<SessionPublisher>>>>,
    store: Arc<dyn LiveReplayStore>,
    /// Durable commits reach the process observation hub as `Committed` items.
    observation_hub: Arc<crate::process_observation::ProcessObservationHub>,
    host_sink: Option<Arc<dyn ProcessEventSink>>,
    forward_host_events: bool,
}

impl ProcessLifecycleFeed {
    pub(crate) fn new(
        store: Arc<dyn LiveReplayStore>,
        observation_hub: Arc<crate::process_observation::ProcessObservationHub>,
        host_sink: Option<Arc<dyn ProcessEventSink>>,
        forward_host_events: bool,
    ) -> Self {
        Self {
            registry: OnceLock::new(),
            routes: Mutex::new(HashMap::new()),
            store,
            observation_hub,
            host_sink,
            forward_host_events,
        }
    }

    pub(crate) fn bind_registry(&self, registry: Arc<dyn ProcessRegistry>) {
        let _ = self.registry.set(registry);
    }

    #[cfg(test)]
    pub(crate) fn route_count(&self) -> usize {
        self.routes.lock_recover().values().map(Vec::len).sum()
    }

    fn release_dead_publisher(&self, session_id: &SessionId, dead: &Arc<SessionPublisher>) {
        let mut routes = self.routes.lock_recover();
        if let Some(publishers) = routes.get_mut(session_id) {
            publishers.retain(|publisher| !Arc::ptr_eq(publisher, dead));
            if publishers.is_empty() {
                routes.remove(session_id);
            }
        }
    }

    pub(crate) fn register(self: &Arc<Self>, handle: &RuntimeHandle) -> Arc<ProcessLifecycleRoute> {
        let observation = handle.observe();
        let session_id = SessionId::from(observation.session_id());
        let weak = Arc::downgrade(&handle.observation);
        let store = Arc::clone(&self.store);
        let route_session_id = session_id.clone();
        let publisher: Arc<SessionPublisher> = Arc::new(move |kind, process_id| {
            let Some(observation) = weak.upgrade() else {
                return false;
            };
            let revision = observation.load_full().session_revision();
            let result = store
                .prepare_publication(
                    &route_session_id,
                    revision,
                    vec![LiveReplayEventDraft::new(
                        None::<String>,
                        SessionObservationEventPayload::ProcessChanged {
                            kind,
                            process_ids: vec![process_id],
                        },
                    )],
                )
                .and_then(|prepared| store.publish_prepared(prepared).map(|_| ()));
            if let Err(error) = result {
                tracing::warn!(session_id = %route_session_id, %error,
                    "failed to publish process lifecycle observation");
            }
            true
        });
        self.routes
            .lock_recover()
            .entry(session_id.clone())
            .or_default()
            .push(Arc::clone(&publisher));
        Arc::new(ProcessLifecycleRoute {
            feed: Arc::downgrade(self),
            session_id,
            publisher,
        })
    }
}

#[async_trait::async_trait]
impl ProcessEventSink for ProcessLifecycleFeed {
    async fn emit(&self, event: &ProcessEvent) {
        self.observation_hub.publish_committed(event);
        if let Some(kind) =
            SessionProcessEventKind::from_durable_event(&event.event_type, event.sequence)
            && !self.routes.lock_recover().is_empty()
            && let Some(registry) = self.registry.get()
        {
            match registry.observers_for_process(&event.process_id).await {
                Ok(observers) => {
                    let routes = {
                        let routes = self.routes.lock_recover();
                        observers
                            .into_iter()
                            .filter_map(|id| routes.get(&id).cloned().map(|route| (id, route)))
                            .collect::<Vec<_>>()
                    };
                    for (session_id, publishers) in routes {
                        for publisher in publishers {
                            if publisher(kind, event.process_id.clone()) {
                                break;
                            }
                            self.release_dead_publisher(&session_id, &publisher);
                        }
                    }
                }
                Err(error) => tracing::warn!(process_id = %event.process_id, %error,
                    "could not route process lifecycle observation"),
            }
        }
        if self.forward_host_events
            && let Some(host_sink) = &self.host_sink
        {
            host_sink.emit(event).await;
        }
    }

    async fn emit_worker_fault(&self, fault: &ProcessWorkerFault) {
        if let Some(host_sink) = &self.host_sink {
            host_sink.emit_worker_fault(fault).await;
        }
    }
}
