//! The suite proves cold rebuild of a trigger-mutated session and durable worker recovery
//! across every `ProcessInput` variant the worker runs.
//!
//! Two backends cover it: a SQLite memory backend and a SQLite file
//! backend. Each supplies every port, the Lashlang artifact store included,
//! from its one location.

use super::*;
use crate::testing::{RuntimeRebuildBackend, runtime_rebuild_and_worker_recovery};

fn backend_over(backend: lash_sqlite_store::SqliteBackend) -> RuntimeRebuildBackend {
    RuntimeRebuildBackend {
        artifact_store: backend.process_env_store(),
        backend: Arc::new(backend),
    }
}

#[test]
fn runtime_rebuild_and_worker_recovery_on_a_memory_backend() {
    run_async_test_on_stack_budget("runtime-rebuild-memory-backend", || async {
        runtime_rebuild_and_worker_recovery(|| async {
            backend_over(
                lash_sqlite_store::SqliteBackend::memory()
                    .await
                    .expect("open the memory backend"),
            )
        })
        .await;
    });
}

#[test]
fn runtime_rebuild_and_worker_recovery_with_durable_stores() {
    run_async_test_on_stack_budget("runtime-rebuild-file-backend", || async {
        let root = tempfile::tempdir().expect("tempdir");
        let scenario = std::sync::atomic::AtomicUsize::new(0);
        runtime_rebuild_and_worker_recovery(|| {
            let dir = root.path().join(format!(
                "scenario-{}",
                scenario.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            async move {
                backend_over(
                    lash_sqlite_store::SqliteBackend::open(dir)
                        .await
                        .expect("open the file backend"),
                )
            }
        })
        .await;
    });
}
