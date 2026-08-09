//! Host-controlled worker-capacity example.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash::{WorkerSlotKind, WorkerSlotPermit, WorkerSlotSupplier};
use tokio::sync::{Semaphore, watch};

/// A minimal host policy: fixed per-lane bounds that admit no new work while
/// an external health signal is closed.
struct ExternallyGatedSlots {
    enabled: watch::Receiver<bool>,
    process: Arc<Semaphore>,
    queued_work: Arc<Semaphore>,
    process_reservations: AtomicUsize,
}

impl ExternallyGatedSlots {
    fn new(enabled: watch::Receiver<bool>, process: usize, queued_work: usize) -> Self {
        Self {
            enabled,
            process: Arc::new(Semaphore::new(process)),
            queued_work: Arc::new(Semaphore::new(queued_work)),
            process_reservations: AtomicUsize::new(0),
        }
    }

    fn semaphore(&self, kind: WorkerSlotKind) -> &Arc<Semaphore> {
        match kind {
            WorkerSlotKind::Process => &self.process,
            WorkerSlotKind::QueuedWork => &self.queued_work,
        }
    }
}

#[async_trait::async_trait]
impl WorkerSlotSupplier for ExternallyGatedSlots {
    async fn reserve_slot(&self, kind: WorkerSlotKind) -> WorkerSlotPermit {
        let mut enabled = self.enabled.clone();
        while !*enabled.borrow_and_update() {
            enabled
                .changed()
                .await
                .expect("capacity signal sender remains alive");
        }
        let permit = Arc::clone(self.semaphore(kind))
            .acquire_owned()
            .await
            .expect("capacity semaphore remains open");
        if kind == WorkerSlotKind::Process {
            self.process_reservations.fetch_add(1, Ordering::SeqCst);
        }
        WorkerSlotPermit::new(permit)
    }

    fn try_reserve_slot(&self, kind: WorkerSlotKind) -> Option<WorkerSlotPermit> {
        if !*self.enabled.borrow() {
            return None;
        }
        let permit = Arc::clone(self.semaphore(kind)).try_acquire_owned().ok()?;
        if kind == WorkerSlotKind::Process {
            self.process_reservations.fetch_add(1, Ordering::SeqCst);
        }
        Some(WorkerSlotPermit::new(permit))
    }

    fn available_slots(&self, kind: WorkerSlotKind) -> usize {
        if *self.enabled.borrow() {
            self.semaphore(kind).available_permits()
        } else {
            0
        }
    }
}

fn builder_with_external_capacity_signal(
    enabled: watch::Receiver<bool>,
) -> (lash::LashCoreBuilder, Arc<ExternallyGatedSlots>) {
    let supplier = Arc::new(ExternallyGatedSlots::new(enabled, 1, 1));
    let builder = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .worker_slot_supplier(supplier.clone());
    (builder, supplier)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn custom_supplier_follows_external_signal_and_releases_by_raii() {
        let (enabled_tx, enabled_rx) = watch::channel(false);
        let (builder, supplier) = builder_with_external_capacity_signal(enabled_rx);

        assert!(supplier.try_reserve_slot(WorkerSlotKind::Process).is_none());
        enabled_tx.send(true).expect("enable capacity");
        let permit = supplier.reserve_slot(WorkerSlotKind::Process).await;
        assert_eq!(supplier.available_slots(WorkerSlotKind::Process), 0);
        drop(permit);
        assert_eq!(supplier.available_slots(WorkerSlotKind::Process), 1);
        assert_eq!(supplier.available_slots(WorkerSlotKind::QueuedWork), 1);

        let registry = Arc::new(
            lash_sqlite_store::SqliteProcessRegistry::memory()
                .await
                .expect("open in-memory process registry"),
        );
        let store_root =
            std::env::temp_dir().join(format!("lash-docs-worker-capacity-{}", std::process::id()));
        let core = builder
            .model(
                lash::ModelSpec::builder("worker-capacity-example")
                    .context_window_tokens(4_096)
                    .build()
                    .expect("valid example model"),
            )
            .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                &store_root,
            )))
            .process_registry(registry.clone())
            .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
            .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
            .process_env_store(Arc::new(
                lash::persistence::InMemoryProcessExecutionEnvStore::new(),
            ))
            .build()
            .expect("build core with custom worker capacity");
        use lash::process::{
            ProcessInput, ProcessProvenance, ProcessRegistration, ProcessRegistry,
            RecoveryDisposition,
        };
        registry
            .register_process(ProcessRegistration::new(
                "worker-capacity-example-process",
                ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                RecoveryDisposition::Rerunnable,
                ProcessProvenance::host(),
            ))
            .await
            .expect("register example process");
        core.session("worker-capacity-example-session")
            .open()
            .await
            .expect("open session and drive process work");
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let process = registry
                    .get_process("worker-capacity-example-process")
                    .await
                    .expect("read example process")
                    .expect("example process remains retained");
                if process.first_started.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the built worker starts the example process");
        assert!(
            supplier.process_reservations.load(Ordering::SeqCst) > 0,
            "the built worker must reserve process capacity from the custom supplier"
        );
        drop(core);
        let _ = std::fs::remove_dir_all(store_root);
    }
}
