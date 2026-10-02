use super::*;
use lash_core::plugin::HydratedExecutionState;

async fn deferred_outcomes_recover_only_from_journal(
    double: lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>,
) {
    let backend = double.lash_backend();
    let calls = Arc::new(AtomicUsize::new(0));
    let installed = Arc::new(AtomicUsize::new(0));
    let resolver: lash_lashlang_runtime::SharedDeferredToolResolver =
        Arc::new(CountingDeferredResolver {
            calls: calls.clone(),
            batches: Arc::new(Mutex::new(Vec::new())),
            installed: installed.clone(),
        });
    let snapshot = Arc::new(std::sync::Mutex::new(None::<HydratedExecutionState>));
    let errors = Arc::new(std::sync::Mutex::new(Vec::new()));
    let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
        let backend = backend.clone();
        let resolver = resolver.clone();
        let snapshot = snapshot.clone();
        let errors = errors.clone();
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let resolver = resolver.clone();
            let snapshot = snapshot.clone();
            let errors = errors.clone();
            Box::pin(async move {
                let mut state = RlmExecutionState::new();
                if !crash {
                    let saved = snapshot
                        .lock()
                        .expect("snapshot lock")
                        .clone()
                        .expect("captured root");
                    state
                        .restore_execution_state(&saved, lash_core::FleetFormat::current())
                        .await
                        .expect("cold restore");
                }
                let ctx = lash_core::testing::code_execution_context_with_invocation(
                    crate::testing::attempt_ports(&backend, scoped),
                    lash_core::testing::exec_code_invocation(
                        "deferred-journal-law",
                        "turn-1",
                        0,
                        0,
                        "exec-code",
                        "exec-code:0",
                    ),
                );
                let result = execute_code_unbounded_with_test_render(
                    &mut state,
                    ctx,
                    deferred_matrix_request(),
                    lashlang::LashlangArtifacts::of_backend(&backend),
                    LashlangSurface::default(),
                    Some(resolver),
                    RlmProjectedBindings::default(),
                    None,
                )
                .await;
                errors
                    .lock()
                    .expect("error log lock")
                    .push(result.error.is_some());
                if crash {
                    let captured = hydrate_snapshot(
                        state
                            .snapshot_execution_state(lash_core::FleetFormat::current())
                            .await
                            .expect("capture root"),
                    );
                    *snapshot.lock().expect("snapshot lock") = Some(captured);
                }
                assert!(
                    !crash,
                    "the deployment dies after recording the link and snapshot"
                );
            })
        })
    };
    double
        .run_crashed_then_redriven(
            lash_core::AdmittedScope::turn("deferred-journal-law", "turn-1"),
            attempt(true),
            attempt(false),
        )
        .await
        .expect("crash and cold redrive");
    let saved = snapshot
        .lock()
        .expect("snapshot lock")
        .clone()
        .expect("captured root");
    let root: BTreeMap<String, serde::de::IgnoredAny> =
        rmp_serde::from_slice(&saved.root).expect("decode root");
    assert!(
        !root.contains_key("deferred_resolutions"),
        "only the journal owns tool outcomes"
    );
    assert_eq!(
        *errors.lock().expect("error log lock"),
        [true, true],
        "mystery.x remains unavailable"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "recover outcomes from journal"
    );
    assert_eq!(
        installed.load(Ordering::SeqCst),
        2,
        "reinstall recorded grant"
    );
}

#[test]
fn deferred_outcomes_recover_only_from_journal_sqlite_memory() {
    block_on(async {
        let double = lash_restate_test::backend_with_store_set(
            SEED,
            Default::default(),
            Default::default(),
            |clock| async move {
                Ok(Arc::new(
                    lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
                        .await
                        .map_err(|e| lash_restate_test::BackendError::Stores(e.to_string()))?,
                ) as Arc<dyn lash_core::StoreSet>)
            },
        )
        .await
        .expect("SQLite memory double");
        deferred_outcomes_recover_only_from_journal(double).await;
    });
}

#[test]
fn deferred_outcomes_recover_only_from_journal_sqlite_file() {
    block_on(async {
        let directory = tempfile::tempdir().expect("SQLite directory");
        let path = directory.path().to_path_buf();
        let double = lash_restate_test::backend_with_store_set(
            SEED,
            Default::default(),
            Default::default(),
            |clock| async move {
                Ok(Arc::new(
                    lash_sqlite_store::SqliteStoreSet::open_with_clock(path, clock)
                        .await
                        .map_err(|e| lash_restate_test::BackendError::Stores(e.to_string()))?,
                ) as Arc<dyn lash_core::StoreSet>)
            },
        )
        .await
        .expect("SQLite file double");
        deferred_outcomes_recover_only_from_journal(double).await;
    });
}

#[test]
#[ignore = "requires a provisioned PostgreSQL service"]
fn deferred_outcomes_recover_only_from_journal_postgres() {
    block_on(async {
        let url = lash_postgres_store::testing::required_database_url();
        let attachments = tempfile::tempdir().expect("attachment directory");
        let bytes = Arc::new(lash_core::facade_support::FileAttachmentStore::new(
            attachments.path(),
        ));
        let double = lash_restate_test::backend_with_store_set(
            SEED,
            Default::default(),
            Default::default(),
            |clock| async move {
                let storage = lash_postgres_store::PostgresStorage::connect(&url)
                    .await
                    .map_err(|e| lash_restate_test::BackendError::Stores(e.to_string()))?;
                Ok(Arc::new(lash_postgres_store::PostgresStoreSet::with_clock(
                    &storage,
                    bytes,
                    Default::default(),
                    clock,
                )) as Arc<dyn lash_core::StoreSet>)
            },
        )
        .await
        .expect("PostgreSQL double");
        deferred_outcomes_recover_only_from_journal(double).await;
    });
}
