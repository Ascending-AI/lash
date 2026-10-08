use crate::ProcessId;
use lash_sansio::sync::MutexExt;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};

use super::model::SessionId;
use super::registry::ProcessRegistry;
use super::registry_delegate::{
    delegate_process_event_log, delegate_process_lifecycle, delegate_process_observer_registry,
    delegate_process_query, delegate_process_registrar, delegate_process_retention,
    delegate_process_tool_intents,
};

mod change_hub;
#[path = "awaiter/event_sink.rs"]
mod event_sink;
mod registry_support;
pub use change_hub::{ProcessChangeHub, ProcessChangeSubscription};
pub use event_sink::ProcessEventSink;

/// [`ProcessRegistry`] decorator: publishes in-process change ticks on every
/// mutation (so native process waits wake without polling) and, when a
/// [`ProcessEventSink`] is installed, emits each appended event to it.
///
/// Sinks share the same watched handle used by the process port.
struct WatchedProcessRegistry {
    inner: Arc<dyn ProcessRegistry>,
    hub: ProcessChangeHub,
    sinks: Arc<Mutex<Vec<Arc<dyn ProcessEventSink>>>>,
    event_paths: Mutex<HashMap<ProcessId, Weak<tokio::sync::Mutex<()>>>>,
    /// The last sequence emitted to the sinks per live process, shared by
    /// every path that emits, so no event reaches a sink twice.
    emitted: Mutex<HashMap<ProcessId, u64>>,
}

/// A process registry paired with the change hub published by its decorator.
///
/// The fields are private so consumers cannot combine a registry from one
/// watch with the hub from another.
#[derive(Clone)]
pub struct WatchedRegistry {
    registry: Arc<dyn ProcessRegistry>,
    watched: Arc<WatchedProcessRegistry>,
    hub: ProcessChangeHub,
    sinks: Arc<Mutex<Vec<Arc<dyn ProcessEventSink>>>>,
}

/// Detaches an observer from a shared watched registry when the last owner drops.
pub struct ProcessEventSinkRegistration {
    sinks: Weak<Mutex<Vec<Arc<dyn ProcessEventSink>>>>,
    sink: Arc<dyn ProcessEventSink>,
}

impl Drop for ProcessEventSinkRegistration {
    fn drop(&mut self) {
        if let Some(sinks) = self.sinks.upgrade() {
            sinks
                .lock_recover()
                .retain(|sink| !Arc::ptr_eq(sink, &self.sink));
        }
    }
}

impl WatchedRegistry {
    fn new(inner: Arc<dyn ProcessRegistry>) -> Self {
        let hub = ProcessChangeHub::new();
        let sinks = Arc::new(Mutex::new(Vec::new()));
        let watched = Arc::new(WatchedProcessRegistry {
            inner: Arc::clone(&inner),
            hub: hub.clone(),
            sinks: Arc::clone(&sinks),
            event_paths: Mutex::new(HashMap::new()),
            emitted: Mutex::new(HashMap::new()),
        });
        let registry: Arc<dyn ProcessRegistry> = watched.clone();
        Self {
            registry,
            watched,
            hub,
            sinks,
        }
    }

    /// The watched registry handle.
    pub fn registry(&self) -> &Arc<dyn ProcessRegistry> {
        &self.registry
    }

    /// The change hub paired with this watched registry.
    pub fn hub(&self) -> &ProcessChangeHub {
        &self.hub
    }

    /// Attach a live event observer to this watched registry and its bound port.
    /// How many event sinks are attached: what the registration-detach law in
    /// `tests/store_backed` reads, since the sink list is private.
    #[cfg(feature = "testing")]
    pub fn event_sink_count_for_testing(&self) -> usize {
        self.sinks.lock_recover().len()
    }

    pub fn add_event_sink(&self, sink: Arc<dyn ProcessEventSink>) -> ProcessEventSinkRegistration {
        self.sinks.lock_recover().push(Arc::clone(&sink));
        ProcessEventSinkRegistration {
            sinks: Arc::downgrade(&self.sinks),
            sink,
        }
    }

    /// Publish what durable commits appended to `process_id`'s log, which
    /// reach the store without passing this registry: a change tick, and to
    /// the sinks, in sequence order, every event no path emitted on this
    /// node yet. `first_after` is where the first emission for the process
    /// starts, once this node emitted nothing of it. Answers the last
    /// sequence read, where the caller's next `first_after` stands, or
    /// `first_after` when nothing was read.
    pub async fn publish_committed(&self, process_id: &ProcessId, first_after: u64) -> u64 {
        let event_path = self.watched.event_path(process_id);
        let _guard = event_path.lock().await;
        self.hub.notify(process_id);
        self.watched
            .emit_event_pages_since(process_id, Some(first_after))
            .await
            .unwrap_or(first_after)
    }
}

/// The decorated handle publishes change ticks to the returned
/// [`ProcessChangeHub`]; [`WatchedRegistry::add_event_sink`] also feeds a
/// host-facing [`ProcessEventSink`].
pub fn watch_process_registry(inner: Arc<dyn ProcessRegistry>) -> WatchedRegistry {
    WatchedRegistry::new(inner)
}

impl crate::FleetFormatStore for WatchedProcessRegistry {
    fn fleet_format(&self) -> crate::FleetFormat {
        self.inner.fleet_format()
    }
}

delegate_process_query!(WatchedProcessRegistry, inner);

delegate_process_registrar!(
    WatchedProcessRegistry,
    inner,
    registration | watched,
    forwarded | {
        let record = forwarded.await?;
        watched.hub.notify(
            crate::runtime::process::registry_delegate::RegisteredProcess::registered_process_id(
                &record,
            ),
        );
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
        watched
            .emit_event_pages_since(process_id, sink_cursor)
            .await;
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
        watched
            .emit_event_pages_since(process_id, sink_cursor)
            .await;
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
        watched
            .emit_event_pages_since(process_id, sink_cursor)
            .await;
        Ok(result)
    }
);

delegate_process_tool_intents!(WatchedProcessRegistry, inner);

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
                sinks: Arc::clone(&self.sinks),
                event_paths: Mutex::new(HashMap::new()),
                emitted: Mutex::new(HashMap::new()),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
