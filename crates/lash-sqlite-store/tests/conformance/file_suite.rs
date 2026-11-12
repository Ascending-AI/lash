//! File-backed registrations for cold reopen and transaction witnesses.
//! The common law corpus runs in conformance_memory.rs.
use super::SUBSTRATE;
use crate::backend_fixture::{Substrate, TestBackend, sync_await};
use lash_conformance::ReopenableProcessRegistry;
use lash_core_store::store::RuntimeStore;
use lash_execution_contracts::deployment::DeploymentStore;
use lash_execution_contracts::process::{
    ProcessEnvStore, ProcessLifecycle as _, ProcessRegistrar as _, ProcessRegistry,
};
use lash_kernel_value::identity::SessionId;
use lash_protocol_types::sync::MutexExt;
use std::sync::{Arc, Mutex};
include!("suite_fixture.rs");
fn artifact_store_handles(
    backend: &TestBackend,
) -> lash_conformance::fused_artifact_store::ArtifactStoreHandles {
    let store = backend.blocking_store();
    lash_conformance::fused_artifact_store::ArtifactStoreHandles {
        packages: Arc::clone(&store) as Arc<dyn lash_durable::packages::PackageStore>,
        package_storage: Arc::clone(&store) as Arc<dyn lash_durable::packages::PackageStorage>,
        artifacts: Arc::clone(&store)
            as Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
        process_env: Arc::clone(&store) as Arc<dyn ProcessEnvStore>,
        turn_preludes: store as Arc<dyn lash_execution_contracts::turn_prelude::TurnPreludeStore>,
    }
}
lash_conformance::abandoned_attachment_recovery_tests!({
    let retained: Retained<TestBackend> = Retained::default();
    let bytes_root = Arc::new(tempfile::tempdir().expect("attachment bytes root"));
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    ((retained.clone(), bytes_root), move || {
        let retained = retained.clone();
        let make_bytes = Arc::clone(&make_bytes);
        async move {
            let backend = TestBackend::open(SUBSTRATE).await;
            retained.keep(&backend);
            let factory = backend.store().await as Arc<dyn DeploymentStore>;
            (factory, make_bytes, move || async move {
                backend.reopen().await.store().await as Arc<dyn DeploymentStore>
            })
        }
    })
});
lash_conformance::checkpoint_component_reopen_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let reopen = backend.clone();
    (backend, move || {
        let reopen = reopen.clone();
        sync_await(async move { reopen.reopen().await }).blocking_store() as Arc<dyn RuntimeStore>
    })
});
lash_conformance::process_projection_repair_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    lash_execution_contracts::testing::process_execution_env_fixture(
        backend.process_env_store().as_ref(),
    )
    .await;
    let registry = backend.process_registry();
    let corruption = backend.clone();
    (
        backend,
        registry as Arc<dyn ProcessRegistry>,
        move |stale: lash_execution_contracts::process::ProcessRecord| async move {
            let conn = corruption.raw();
            let changed = conn
                .execute(
                    "UPDATE processes SET lifecycle_json = ?2 WHERE process_id = ?1",
                    rusqlite::params![
                        stale.id.as_str(),
                        serde_json::to_string(&stale.lifecycle_storage())
                            .expect("encode stale process projection")
                    ],
                )
                .expect("corrupt SQLite process projection");
            assert_eq!(changed, 1);
        },
    )
});
lash_conformance::artifact_store_reopenable_tests!(@catalogue {
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move || {
        let backend = retained.open_blocking();
        let keep_reopen = retained.clone();
        lash_conformance::fused_artifact_store::ReopenableArtifactStore {
            open: artifact_store_handles(&backend),
            reopen: Arc::new(move || {
                let source = backend.clone();
                let reopened = sync_await(async move { source.reopen().await });
                keep_reopen.keep(&reopened);
                artifact_store_handles(&reopened)
            }),
            interrupt: None,
        }
    })
}; [

            (lash_vm_artifact_survives_reopen, "lash-vm-artifact-reopen"),

            (process_env_survives_reopen, "process-env-reopen"),

        ]);
lash_conformance::process_registry_reopenable_tests!(@reopen {
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move |_label: &str| {
        let backend = retained.open_blocking();
        sync_await({
            let backend = backend.clone();
            async move {
                lash_conformance::publish_process_registry_fixture_environments(
                    backend.process_env_store().as_ref(),
                )
                .await;
            }
        });
        let reopened = sync_await({
            let backend = backend.clone();
            async move { backend.reopen().await }
        });
        retained.keep(&reopened);
        ReopenableProcessRegistry {
            open: backend.process_registry()
                as Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry>,
            reopen: reopened.process_registry()
                as Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry>,
        }
    })
}; [(process_registry_reopen_conformance, "process-registry-reopen")]);

lash_conformance::__process_registry_register! {
            reopenable {
    let retained: Retained<TestBackend> = Retained::default();
    (retained.clone(), move |_label: &str| {
        let backend = retained.open_blocking();
        sync_await({
            let backend = backend.clone();
            async move {
                lash_conformance::publish_process_registry_fixture_environments(
                    backend.process_env_store().as_ref(),
                )
                .await;
            }
        });
        let reopened = sync_await({
            let backend = backend.clone();
            async move { backend.reopen().await }
        });
        retained.keep(&reopened);
        ReopenableProcessRegistry {
            open: backend.process_registry()
                as Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry>,
            reopen: reopened.process_registry()
                as Arc<dyn lash_execution_contracts::process::ConformanceProcessRegistry>,
        }
    })
};
            probe [

            ]
            conformance [

                (a_failed_observer_transfer_leaves_no_partial_mutation, "observer-transfer-rollback"),
            ]
            cancellation [

            ]
            registry [

                (process_change_feed_never_misses_concurrent_terminal_writers, "concurrent-terminal-feed"),

            ]
        }

lash_conformance::checkpoint_profile_tests!(@law {
    let first = TestBackend::open_with(
        SUBSTRATE,
        |mut options| {
            options.store.blob_profile = lash_sqlite_store::BuiltinBlobProfile::LowLatency;
            options
        },
        Arc::new(lash_core_ids::test_clock::TestClock::new(10_000)),
    )
    .await;
    let balanced = first
        .reopen_with(
            |mut options| {
                options.store.blob_profile = lash_sqlite_store::BuiltinBlobProfile::Balanced;
                options
            },
            Arc::new(lash_core_ids::test_clock::TestClock::new(10_000)),
        )
        .await;
    let compact = first
        .reopen_with(
            |mut options| {
                options.store.blob_profile = lash_sqlite_store::BuiltinBlobProfile::Compact;
                options
            },
            Arc::new(lash_core_ids::test_clock::TestClock::new(10_000)),
        )
        .await;
    let stores = vec![
        first.store().await as Arc<dyn RuntimeStore>,
        balanced.store().await,
        compact.store().await,
    ];
    ((first, balanced, compact), stores)
}; checkpoint_profile_change_preserves_refs_budget_and_atomic_root_leaves);
mod process_retention {
    use super::*;
    use lash_execution_contracts::process::ProcessRetention as _;
    include!("process_retention_reopen.rs");
}
mod session_mail {
    include!("session_mail_reopen.rs");
}
mod attachment_store {
    use super::*;
    use lash_core_store::attachments::{AttachmentStore, AttachmentStorePersistence};
    include!("attachment_reopen.rs");
}
mod durable_laws {
    use super::*;
    use lash_conformance::ReopenableRuntimeStore;
    use lash_core_execution::ActorContext;
    use lash_core_store::store::SessionCatalogStore as _;
    /// The effect context a law's turns run on: the durable backend over
    /// `backend`'s store set, owning no actor.
    fn durable_host(backend: &TestBackend) -> ActorContext {
        ActorContext::detached(backend.as_backend())
    }
    mod runtime_persistence {
        use super::*;
        lash_conformance::runtime_persistence_reopenable_tests!(@reopen_laws {
        let retained: Retained<TestBackend> = Retained::default();
        let clock = Arc::new(lash_core_ids::test_clock::TestClock::new(10_000));
        let store_clock = Arc::clone(&clock);
        (
            retained.clone(),
            move |session_id: &str| {
                let request = root_session_request(session_id);
                let clock = store_clock.clone() as Arc<dyn lash_core_ids::clock::Clock>;
                let (backend, open, reopen) = sync_await(async move {
                    let backend = TestBackend::open_with_clock(SUBSTRATE, clock).await;
                    let open = backend.store().await;
                    open.admit_session(&request)
                        .await
                        .expect("admit SQLite conformance session");
                    let reopen = backend.reopen().await.store().await;
                    (backend, open, reopen)
                });
                let effect_host = durable_host(&backend);
                retained.keep(&backend);
                ReopenableRuntimeStore {
                    open: open as Arc<dyn RuntimeStore>,
                    reopen: reopen as Arc<dyn RuntimeStore>,
                    effect_host,
                }
            },
            lash_conformance::RuntimePersistenceLeaseTiming::controlled({
                let clock = Arc::clone(&clock);
                move |duration_ms| clock.advance(duration_ms)
            }),
        )
    }; [(append_receipt_reopen, "root"), (reopen_mint_identity, "pending-turn-input-multi-store-mint")]);
    }
}

lash_conformance::attachment_condemnation_recovery_tests!({
    let backend = TestBackend::open(SUBSTRATE).await;
    let factory = backend.store().await;
    let reopen = backend.clone();
    let bytes_root = tempfile::tempdir().expect("attachment bytes root");
    let make_bytes = crate::backend_fixture::attachment_bytes(&bytes_root);
    (
        (backend, bytes_root),
        factory,
        make_bytes,
        move || async move { reopen.reopen().await.store().await as Arc<dyn DeploymentStore> },
    )
});

/// Handles over a file store set opened on `path` alone, with `pauses` in
/// its connections. Nothing else keeps the store set, so dropping the
/// handles closes the database.
fn file_artifact_handles(
    path: &std::path::Path,
    pauses: &lash_sqlite_store::testing::SqlitePauses,
) -> lash_conformance::fused_artifact_store::ArtifactStoreHandles {
    let path = path.to_owned();
    let pauses = pauses.clone();
    let store = sync_await(async move {
        lash_sqlite_store::SqliteStoreSet::open_with_options_and_clock(
            path,
            lash_sqlite_store::SqliteStoreSetOptions {
                pauses: Some(pauses),
                ..lash_sqlite_store::SqliteStoreSetOptions::standard(
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
            },
            Arc::new(lash_core_ids::clock::SystemClock),
        )
        .await
        .expect("open the file store set")
        .open_store()
        .await
        .expect("open the file store")
    });
    lash_conformance::fused_artifact_store::ArtifactStoreHandles {
        packages: Arc::clone(&store) as Arc<dyn lash_durable::packages::PackageStore>,
        package_storage: Arc::clone(&store) as Arc<dyn lash_durable::packages::PackageStorage>,
        artifacts: Arc::clone(&store)
            as Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>,
        process_env: Arc::clone(&store) as Arc<dyn ProcessEnvStore>,
        turn_preludes: store as Arc<dyn lash_execution_contracts::turn_prelude::TurnPreludeStore>,
    }
}

/// Wait until this process holds no descriptor on the database at `path`
/// or its write-ahead log: the dropped store set has closed them.
fn await_closed(path: &std::path::Path) {
    let names: Vec<std::path::PathBuf> = ["", "-wal", "-shm"]
        .iter()
        .map(|suffix| {
            let mut name = path.as_os_str().to_owned();
            name.push(suffix);
            std::path::PathBuf::from(name)
        })
        .collect();
    let open = || {
        std::fs::read_dir("/proc/self/fd")
            .expect("list this process's descriptors")
            .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
            .any(|target| names.contains(&target))
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while open() {
        assert!(
            std::time::Instant::now() < deadline,
            "the store set never closed {}",
            path.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A reopen of the file at `path`: the previous handles' database is
/// closed first, so the reopened store reads only what the file holds.
fn reopen_file(
    path: std::path::PathBuf,
    pauses: lash_sqlite_store::testing::SqlitePauses,
) -> lash_conformance::fused_artifact_store::Reopen {
    Arc::new(move || {
        await_closed(&path);
        file_artifact_handles(&path, &pauses)
    })
}

// The module-share laws over a real file close and reopen between the two
// ends, and a crash while an end's transaction is open: the store's writer
// pauses with its transaction begun and fenced, the database and its log are
// copied as a crash there leaves them, and the recovery opens the copy
// (FIG-7700).
lash_conformance::artifact_referrer_tests!(@catalogue {
    let kept: Retained<Arc<tempfile::TempDir>> = Retained::default();
    (kept.clone(), move || {
        let dir = Arc::new(tempfile::tempdir().expect("file store tempdir"));
        kept.keep(&dir);
        let path = dir.path().join("lash.db");
        let pauses = lash_sqlite_store::testing::SqlitePauses::default();
        let kept = kept.clone();
        let interrupt_pauses = pauses.clone();
        let interrupt_path = path.clone();
        lash_conformance::fused_artifact_store::ReopenableArtifactStore {
            open: file_artifact_handles(&path, &pauses),
            reopen: reopen_file(path, pauses),
            interrupt: Some(Arc::new(move || {
                let pause = interrupt_pauses.pause_after_fence();
                let source = interrupt_path.clone();
                let crashed = Arc::new(tempfile::tempdir().expect("crash copy tempdir"));
                kept.keep(&crashed);
                Box::pin(async move {
                    pause.wait_until_reached().await;
                    let target = crashed.path().join("lash.db");
                    for suffix in ["", "-wal"] {
                        let mut from = source.as_os_str().to_owned();
                        from.push(suffix);
                        let mut to = target.as_os_str().to_owned();
                        to.push(suffix);
                        if std::path::Path::new(&from).exists() {
                            std::fs::copy(&from, &to).expect("copy the crashed database");
                        }
                    }
                    pause.release();
                    reopen_file(target, lash_sqlite_store::testing::SqlitePauses::default())
                })
                    as std::pin::Pin<
                        Box<
                            dyn std::future::Future<
                                    Output = lash_conformance::fused_artifact_store::Reopen,
                                > + Send,
                        >,
                    >
            })),
        }
    })
}; [
    (module_share_outlives_environment_share_in_either_order, "module-share-end-orders"),
    (module_share_survives_a_cut_between_its_ends, "module-share-crash-cuts"),
]);
