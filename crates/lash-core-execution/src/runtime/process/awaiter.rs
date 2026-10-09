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
/// [`ProcessEventSink`] is installed, emits each appended event to it. Once
/// it announces through a node's hints
/// ([`WatchedRegistry::announce_through`]), an append here also ticks the
/// change hubs of the other nodes, and theirs tick this one's.
///
/// Sinks share the same watched handle used by the process port.
struct WatchedProcessRegistry {
    inner: Arc<dyn ProcessRegistry>,
    hub: ProcessChangeHub,
    sinks: Arc<Mutex<Vec<Arc<dyn ProcessEventSink>>>>,
    publication: Arc<Publication>,
}

/// What every path that emits a process's events on this node shares.
#[derive(Default)]
struct Publication {
    /// One lock per process: its emissions run one at a time, in sequence
    /// order.
    event_paths: Mutex<HashMap<ProcessId, Weak<tokio::sync::Mutex<()>>>>,
    /// The last sequence emitted to the sinks per process, which only moves
    /// forward: no event reaches a sink twice. A mark outlives its
    /// process's terminal and is dropped only once the durable mark covers
    /// it ([`WatchedRegistry::forget_published`]).
    emitted: Mutex<HashMap<ProcessId, u64>>,
    /// The durable marks a process activation records from this node's
    /// marks, once one does: where a path with no mark here starts, so a
    /// mark is always contiguous with what the durable one covers.
    durable: std::sync::OnceLock<Arc<dyn lash_durable::DurableReads>>,
    /// Only degraded reads occupy this map; successful absence is healthy.
    read_failures: Mutex<HashMap<(ProcessId, &'static str), u64>>,
    /// The node hints an append is announced to the other nodes through,
    /// once the composition names them.
    hints: std::sync::OnceLock<lash_durable::runner::Hints>,
}

/// Ticks a change hub for the process logs another node appended to.
struct HubFollowers(ProcessChangeHub);

impl lash_durable::runner::LogFollowers for HubFollowers {
    fn appended(&self, actors: &[lash_durable::ActorKey]) {
        for actor in actors {
            if actor.kind() == lash_durable::ActorKind::Process
                && let Ok(process) = ProcessId::parse(actor.id())
            {
                self.0.notify(&process);
            }
        }
    }

    fn resubscribed(&self) {
        self.0.notify_all();
    }
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
struct ProcessEventSinkRegistration {
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
            publication: Arc::default(),
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

    /// How many event sinks are attached: what the registration-detach law in
    /// `tests/store_backed` reads, since the sink list is private.
    #[cfg(feature = "testing")]
    pub fn event_sink_count_for_testing(&self) -> usize {
        self.sinks.lock_recover().len()
    }

    /// Attach a live event observer to this watched registry and its bound port.
    /// The returned guard detaches the observer on drop and can outlive this handle.
    pub fn add_event_sink(&self, sink: Arc<dyn ProcessEventSink>) -> impl Send + Sync + use<> {
        self.sinks.lock_recover().push(Arc::clone(&sink));
        ProcessEventSinkRegistration {
            sinks: Arc::downgrade(&self.sinks),
            sink,
        }
    }

    /// Publish what durable commits appended to `process_id`'s log, which
    /// reach the store without passing this registry: a change tick, and to
    /// the sinks, in sequence order, every event after `published` (the
    /// process's durable publication mark) that no path emitted on this
    /// node yet. Answers the last sequence emitted on this node, or
    /// `published` when nothing was read.
    pub async fn publish_committed(&self, process_id: &ProcessId, published: u64) -> u64 {
        let event_path = self.watched.event_path(process_id);
        let _guard = event_path.lock().await;
        self.watched.appended(process_id);
        self.watched
            .emit_event_pages_since(process_id, Some(published), published)
            .await
            .unwrap_or(published)
    }

    /// The last sequence of `process_id` emitted on this node: what its
    /// owner records as the durable publication mark.
    #[must_use]
    pub(crate) fn published_mark(&self, process_id: &ProcessId) -> Option<u64> {
        self.watched
            .publication
            .emitted
            .lock_recover()
            .get(process_id)
            .copied()
    }

    /// Drop this node's mark of `process_id` once the durable mark,
    /// `published`, covers it: a later path here starts from the durable
    /// mark, which only moves forward, so it emits nothing again.
    pub(crate) async fn forget_published(&self, process_id: &ProcessId, published: u64) {
        let event_path = self.watched.event_path(process_id);
        let _guard = event_path.lock().await;
        let mut marks = self.watched.publication.emitted.lock_recover();
        if marks.get(process_id).is_some_and(|mark| *mark <= published) {
            marks.remove(process_id);
        }
    }

    /// Announce every append to a process's log to the other nodes of
    /// `hints`' store, and tick this registry's change hub for theirs. The
    /// first hints named stay: a registry serves one backend.
    pub fn announce_through(&self, hints: lash_durable::runner::Hints) {
        if self.watched.publication.hints.set(hints.clone()).is_ok() {
            hints.follow(Arc::new(HubFollowers(self.hub.clone())));
        }
    }

    /// Read the durable marks a process activation records from this
    /// registry's marks from `reads`: a path with no mark for a process
    /// starts from its durable mark, never past it.
    pub(crate) fn read_durable_marks(&self, reads: Arc<dyn lash_durable::DurableReads>) {
        let _ = self.watched.publication.durable.set(reads);
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
        watched.appended(process_id);
        watched
            .emit_event_pages_since(process_id, sink_cursor, 0)
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
        watched.appended(process_id);
        watched
            .emit_event_pages_since(process_id, sink_cursor, 0)
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
        watched.appended(process_id);
        watched
            .emit_event_pages_since(process_id, sink_cursor, 0)
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
                publication: Arc::clone(&self.publication),
            }) as Arc<dyn ProcessRegistry>
        })
    }
}
