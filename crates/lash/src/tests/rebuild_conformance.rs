//! Runs the public
//! [`runtime_rebuild_and_worker_recovery`](crate::testing::conformance::runtime_rebuild_and_worker_recovery)
//! conformance suite against this crate's store backends. The suite proves cold
//! rebuild of a trigger-mutated session and durable worker recovery across every
//! `ProcessInput` variant the worker runs.
//!
//! Two backends cover the explicit-facet API: a fully inline backend using
//! in-memory stores/registry and a fully durable backend using SQLite/file
//! stores/registry. Peer coherence rejects mixed durable session stores with
//! inline attachment or artifact stores, so the cases stay tier-consistent.

use super::*;
use crate::testing::conformance::{RuntimeRebuildBackend, runtime_rebuild_and_worker_recovery};

fn sync_await<T, F>(future: F) -> T
where
    T: Send + 'static,
    F: std::future::Future<Output = T> + Send + 'static,
{
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(future)
    })
    .join()
    .expect("runtime thread")
}

type FreshSqliteSessionBackend = (
    std::path::PathBuf,
    Arc<dyn lash_core::SessionStoreFactory>,
    Arc<dyn lash_core::ProcessRegistry>,
    Arc<dyn lash_core::TriggerStore>,
);

fn fresh_sqlite_session_backend(root: &std::path::Path) -> FreshSqliteSessionBackend {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SCENARIO: AtomicUsize = AtomicUsize::new(0);
    let dir = root.join(format!(
        "scenario-{}",
        SCENARIO.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create scenario dir");
    let store_factory = Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
        dir.join("sessions"),
    )) as Arc<dyn lash_core::SessionStoreFactory>;
    let process_db = dir.join("processes.db");
    let process_sessions = dir.join("sessions");
    let registry = Arc::new(sync_await(async move {
        lash_sqlite_store::SqliteProcessRegistry::open(&process_db, process_sessions)
            .await
            .expect("open process registry")
    })) as Arc<dyn lash_core::ProcessRegistry>;
    let triggers_db = dir.join("triggers.db");
    let trigger_store = Arc::new(sync_await(async move {
        lash_sqlite_store::SqliteTriggerStore::open(&triggers_db)
            .await
            .expect("open trigger store")
    })) as Arc<dyn lash_core::TriggerStore>;
    (dir, store_factory, registry, trigger_store)
}

fn fresh_in_memory_backend() -> (
    Arc<dyn lash_core::SessionStoreFactory>,
    Arc<dyn lash_core::ProcessRegistry>,
    Arc<dyn lash_core::TriggerStore>,
) {
    (
        Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new())
            as Arc<dyn lash_core::SessionStoreFactory>,
        Arc::new(lash_core::TestLocalProcessRegistry::default())
            as Arc<dyn lash_core::ProcessRegistry>,
        Arc::new(lash_core::facade_support::InMemoryTriggerStore::default())
            as Arc<dyn lash_core::TriggerStore>,
    )
}

#[test]
fn runtime_rebuild_and_worker_recovery_with_inline_stores() {
    run_async_test_on_stack_budget("runtime-rebuild-inline-stores", || async {
        runtime_rebuild_and_worker_recovery(move || {
            let (store_factory, registry, trigger_store) = fresh_in_memory_backend();
            RuntimeRebuildBackend {
                process_registry: registry,
                artifact_store: Arc::new(crate::persistence::InMemoryLashlangArtifactStore::new()),
                build_core: Box::new(move |builder| {
                    explicit_ephemeral_facets(builder)
                        .store_factory(Arc::clone(&store_factory))
                        .trigger_store(Arc::clone(&trigger_store))
                        .build()
                        .expect("build core")
                }),
            }
        })
        .await;
    });
}

#[test]
fn runtime_rebuild_and_worker_recovery_with_durable_stores() {
    run_async_test_on_stack_budget("runtime-rebuild-durable-stores", || async {
        let root = tempfile::tempdir().expect("tempdir");
        let root_path = root.path().to_path_buf();
        runtime_rebuild_and_worker_recovery(move || {
            let (dir, store_factory, registry, trigger_store) =
                fresh_sqlite_session_backend(&root_path);
            let attachment = Arc::new(crate::persistence::FileAttachmentStore::new(
                dir.join("attachments"),
            )) as Arc<dyn lash_core::AttachmentStore>;
            let artifact_db = dir.join("artifacts.db");
            let artifact_store = Arc::new(sync_await(async move {
                lash_sqlite_store::Store::open(&artifact_db)
                    .await
                    .expect("open durable artifact store")
            }));
            let artifact = Arc::clone(&artifact_store)
                as Arc<dyn lash_lashlang_runtime::LashlangArtifactStore>;
            let process_env_store =
                Arc::clone(&artifact_store) as Arc<dyn lash_core::ProcessExecutionEnvStore>;
            RuntimeRebuildBackend {
                process_registry: registry,
                artifact_store: Arc::clone(&artifact),
                build_core: Box::new(move |builder| {
                    builder
                        .store_factory(Arc::clone(&store_factory))
                        .attachment_store(Arc::clone(&attachment))
                        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
                        .process_env_store(Arc::clone(&process_env_store))
                        .trigger_store(Arc::clone(&trigger_store))
                        .effect_host(Arc::new(crate::durability::InlineEffectHost::default()))
                        .build()
                        .expect("build core")
                }),
            }
        })
        .await;
    });
}
