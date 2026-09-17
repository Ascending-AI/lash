use crate::ProcessId;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use super::model::SessionId;
use super::registry::ProcessRegistry;
use super::registry_delegate::{
    delegate_process_event_log, delegate_process_leases, delegate_process_lifecycle,
    delegate_process_observer_registry, delegate_process_query, delegate_process_registrar,
    delegate_process_retention, delegate_process_tool_intents, delegate_process_wake_outbox,
};

mod change_hub;
#[path = "awaiter/event_sink.rs"]
mod event_sink;
mod registry_support;
pub use change_hub::ProcessChangeHub;
pub use event_sink::ProcessEventSink;

/// [`ProcessRegistry`] decorator: publishes in-process change ticks on every
/// mutation (so native process waits wake without polling) and, when a
/// [`ProcessEventSink`] is installed, emits each appended event to it.
///
/// The sink is installed once at wrap time via
/// [`watch_process_registry_with_sink`]; there is no post-hoc mutation and no
/// double-wrapping.
struct WatchedProcessRegistry {
    inner: Arc<dyn ProcessRegistry>,
    hub: ProcessChangeHub,
    sink: Option<Arc<dyn ProcessEventSink>>,
    event_paths: Mutex<HashMap<ProcessId, Weak<tokio::sync::Mutex<()>>>>,
}

/// A process registry paired with the change hub published by its decorator.
///
/// The fields are private so consumers cannot combine a registry from one
/// watch with the hub from another.
#[derive(Clone)]
pub struct WatchedRegistry {
    registry: Arc<dyn ProcessRegistry>,
    hub: ProcessChangeHub,
}

impl WatchedRegistry {
    fn new(inner: Arc<dyn ProcessRegistry>, sink: Option<Arc<dyn ProcessEventSink>>) -> Self {
        let hub = ProcessChangeHub::new();
        let registry: Arc<dyn ProcessRegistry> = Arc::new(WatchedProcessRegistry {
            inner: Arc::clone(&inner),
            hub: hub.clone(),
            sink: sink.clone(),
            event_paths: Mutex::new(HashMap::new()),
        });
        Self { registry, hub }
    }

    /// The watched registry handle.
    pub fn registry(&self) -> &Arc<dyn ProcessRegistry> {
        &self.registry
    }

    /// The change hub paired with this watched registry.
    pub fn hub(&self) -> &ProcessChangeHub {
        &self.hub
    }
}

/// Wrap `inner` in a change-publishing registry decorator with no event sink.
///
/// The decorated handle publishes change ticks to the returned
/// [`ProcessChangeHub`]. Use [`watch_process_registry_with_sink`] to also feed a
/// host-facing [`ProcessEventSink`].
pub fn watch_process_registry(inner: Arc<dyn ProcessRegistry>) -> WatchedRegistry {
    watch_process_registry_with_sink(inner, None)
}

/// Wrap `inner` in a change-publishing registry decorator, optionally
/// installing a [`ProcessEventSink`] that receives every appended event.
///
/// The sink is best-effort freshness, not truth — see [`ProcessEventSink`].
pub fn watch_process_registry_with_sink(
    inner: Arc<dyn ProcessRegistry>,
    sink: Option<Arc<dyn ProcessEventSink>>,
) -> WatchedRegistry {
    WatchedRegistry::new(inner, sink)
}

delegate_process_query!(WatchedProcessRegistry, inner);

delegate_process_registrar!(
    WatchedProcessRegistry,
    inner,
    registration | watched,
    process_id,
    forwarded | {
        let record = forwarded.await?;
        watched.hub.notify(&process_id);
        Ok(record)
    },
    event | watched,
    process_id,
    forwarded | {
        let event_path = watched.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = watched.sink_cursor(process_id).await;
        let record = forwarded.await?;
        watched.hub.notify(process_id);
        watched.emit_events_after(process_id, sink_cursor).await;
        Ok(record)
    }
);

delegate_process_observer_registry!(WatchedProcessRegistry, inner);

delegate_process_event_log!(
    WatchedProcessRegistry,
    inner,
    event | watched,
    process_id,
    forwarded | {
        let event_path = watched.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = watched.sink_cursor(process_id).await;
        let result = forwarded.await?;
        watched.hub.notify(process_id);
        watched.emit_events_after(process_id, sink_cursor).await;
        Ok(result)
    }
);

delegate_process_lifecycle!(
    WatchedProcessRegistry,
    inner,
    event | watched,
    process_id,
    forwarded | {
        let event_path = watched.event_path(process_id);
        let _guard = event_path.lock().await;
        let sink_cursor = watched.sink_cursor(process_id).await;
        let result = forwarded.await?;
        watched.hub.notify(process_id);
        watched.emit_events_after(process_id, sink_cursor).await;
        Ok(result)
    }
);

delegate_process_tool_intents!(WatchedProcessRegistry, inner);

delegate_process_wake_outbox!(WatchedProcessRegistry, inner);

delegate_process_leases!(WatchedProcessRegistry, inner);

// No hub bump on retention: pruned rows are terminal, so any waiter on
// them resolved long ago (terminal state is durable and observed via the
// await seam).
delegate_process_retention!(WatchedProcessRegistry, inner);

impl super::registry::ProcessClockRebind for WatchedProcessRegistry {
    fn with_runtime_clock(&self, clock: Arc<dyn crate::Clock>) -> Option<Arc<dyn ProcessRegistry>> {
        self.inner.with_runtime_clock(clock).map(|inner| {
            Arc::new(Self {
                inner,
                hub: self.hub.clone(),
                sink: self.sink.clone(),
                event_paths: Mutex::new(HashMap::new()),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
