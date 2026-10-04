//! L5: recovery publishes and adopts a complete transition before capabilities exist.
use super::*;
use lash_sansio::sync::MutexExt;
use std::sync::{Mutex, Weak};

#[path = "publication_support.rs"]
mod support;
use support::{Counts, Probe, Stores};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cut {
    BeforeConversion,
    ConvertedBeforeRecord,
    RecordedBeforeAck,
    BeforePublication,
    PublishedBeforeReply,
    PublishedThenHeadAdvanced,
    PublishedThenFreshShift,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Publish,
    Stale,
    Refuse,
}

async fn matrix(stores: Stores) {
    let double = lash_restate_test::backend(
        0x4916_0001,
        lash_restate_test::ServerConfig {
            protocol: lash_restate_test::protocol::ProtocolVersion::V7,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    let backend = double.lash_backend();
    for verdict in [Verdict::Publish, Verdict::Stale, Verdict::Refuse] {
        for cut in [
            Cut::RecordedBeforeAck,
            Cut::BeforeConversion,
            Cut::ConvertedBeforeRecord,
            Cut::BeforePublication,
            Cut::PublishedBeforeReply,
        ] {
            scenario(&double, &backend, &stores, verdict, cut).await;
        }
    }
    scenario(
        &double,
        &backend,
        &stores,
        Verdict::Publish,
        Cut::PublishedThenHeadAdvanced,
    )
    .await;
    scenario(
        &double,
        &backend,
        &stores,
        Verdict::Publish,
        Cut::PublishedThenFreshShift,
    )
    .await;
}

async fn scenario(
    double: &lash_restate_test::RestateTestBackend<lash_sqlite_store::SqliteStoreSet>,
    backend: &crate::Backend,
    stores: &Stores,
    verdict: Verdict,
    cut: Cut,
) {
    let id = crate::SessionId::fixture(format!("publication-{verdict:?}-{cut:?}"));
    let counts = Arc::new(Counts::default());
    let mut factories = crate::testing::test_standard_protocol_factories();
    for convert in [false, true] {
        factories.push(Arc::new(Probe {
            counts: counts.clone(),
            convert,
            refuse: verdict == Verdict::Refuse,
        }));
    }
    let host = crate::PluginHost::new(factories);
    let last_store = Arc::new(Mutex::new(None));
    let (initial, request, fence) = {
        let stores = stores.open(&last_store).await;
        let factory = stores.session_store_factory();
        factory
            .admit_session(&root_session_request(&id))
            .await
            .unwrap();
        let raw: Arc<dyn crate::RuntimeStore> = factory;
        let store = crate::store::SessionStore::new(raw.clone(), id.clone()).unwrap();
        let mut initial =
            crate::store::load_session_window_state(&store, crate::store::WindowSelector::Current)
                .await
                .unwrap()
                .unwrap()
                .state;
        initial.policy = crate::testing::standard_test_policy();
        let target = host.admit_plugins(raw.as_ref()).await.unwrap();
        initial.set_plugin_state(Some(crate::PluginState {
            plugins: BTreeMap::from([
                (
                    "convert-probe".into(),
                    crate::PluginNamespaceState {
                        format_version: crate::FormatVersion::ONE,
                        generation: 7,
                        publication: Default::default(),
                        values: BTreeMap::from([("old".into(), serde_json::json!(17))]),
                    },
                ),
                (
                    "inactive".into(),
                    crate::PluginNamespaceState {
                        format_version: crate::FormatVersion::ONE,
                        generation: 9,
                        publication: Default::default(),
                        values: BTreeMap::from([("retained".into(), serde_json::json!(23))]),
                    },
                ),
            ]),
        }));
        initial.authority.plugin_config.insert_versioned(
            "convert-probe",
            crate::FormatVersion::ONE,
            serde_json::json!({"old":17}),
        );
        let receipt = raw
            .commit_runtime_state(crate::RuntimeCommit::persisted_state_for_test(&initial))
            .await
            .unwrap();
        initial.apply_persisted_commit_result(receipt);
        let initial =
            crate::store::load_session_window_state(&store, crate::store::WindowSelector::Current)
                .await
                .unwrap()
                .unwrap()
                .state;
        let request = crate::plugin::PluginTransitionRequest {
            id: crate::plugin::PluginTransitionId(
                crate::EffectAddress::new(
                    crate::ExecutionScope::turn(&id, "run"),
                    "plugin-transition",
                )
                .unwrap(),
            ),
            owner: crate::RuntimeOwner::Session(id.clone()),
            base: crate::plugin::PluginTransitionBase::Session {
                head: crate::store::SessionHeadRef {
                    generation: 0,
                    revision: initial.head_revision,
                    leaf: initial.session_graph.leaf_node_id.clone(),
                    checkpoint: initial.checkpoint_ref.clone(),
                },
            },
            target,
        };
        let fence = seal_shift_fence_for_test(&raw, &id, "first").await;
        (initial, request, fence)
    };
    let attempts = Arc::new(AtomicUsize::new(0));
    let published_head = Arc::new(Mutex::new(None));
    let resume_head = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let stores = stores.clone();
        let backend = backend.clone();
        let counts = counts.clone();
        let attempts = attempts.clone();
        let published_head = published_head.clone();
        let resume_head = resume_head.clone();
        Arc::new(move |controller| {
            let stores = stores.clone();
            let backend = backend.clone();
            let host = host.isolated_registry();
            let initial = initial.clone();
            let request = request.clone();
            let fence = fence.clone();
            let counts = counts.clone();
            let attempts = attempts.clone();
            let published_head = published_head.clone();
            let resume_head = resume_head.clone();
            let last_store = last_store.clone();
            Box::pin(async move {
                let first = attempts.fetch_add(1, Ordering::SeqCst) == 0;
                let opened = stores.open(&last_store).await;
                let raw: Arc<dyn crate::RuntimeStore> = opened.session_store_factory();
                let store =
                    crate::store::SessionStore::new(raw.clone(), initial.session_id.clone())
                        .unwrap();
                let provider_calls = counts.clone();
                let provider = crate::testing::TestProvider::builder()
                    .complete(move |_| {
                        provider_calls.providers.fetch_add(1, Ordering::SeqCst);
                        async { panic!("transition publication must not call a provider") }
                    })
                    .build();
                let mut runtime = Box::pin(
                    crate::LashRuntime::builder(
                        crate::testing::runtime_helpers::test_runtime_host_config_with_provider(
                            &backend,
                            provider.into_handle(),
                        ),
                        crate::testing::runtime_lease_owner(),
                    )
                    .with_initial_state(initial.clone())
                    .with_store(store.clone())
                    .with_plugin_host(host.clone())
                    .build(),
                )
                .await
                .unwrap();
                assert!(
                    runtime.session.is_none(),
                    "the old head has no admitted capabilities"
                );
                if first && cut == Cut::BeforeConversion {
                    panic!("before conversion");
                }
                let resume = if cut == Cut::PublishedThenFreshShift {
                    resume_head.lock_recover().clone()
                } else {
                    None
                };
                let record = record_resuming(
                    &controller,
                    host.clone(),
                    store.clone(),
                    initial.clone(),
                    request.clone(),
                    resume.clone().map(|head| (head, fence.clone())),
                )
                .await
                .unwrap();
                if resume.is_some() {
                    assert!(
                        record.publication.is_none(),
                        "a fresh shift adopts the published transition without a new commit"
                    );
                }
                if first && cut == Cut::BeforePublication {
                    assert_eq!(
                        store
                            .load_session_head_meta()
                            .await
                            .unwrap()
                            .unwrap()
                            .head_revision,
                        initial.head_revision
                    );
                    assert!(runtime.session.is_none());
                    panic!("durable transition before publication");
                }
                match verdict {
                    Verdict::Refuse => {
                        assert!(matches!(
                            record.candidate(),
                            Err(crate::PluginError::Format(crate::FormatRefusal {
                                namespace: crate::FormatNamespace::State,
                                ..
                            }))
                        ));
                        assert!(record.publication.is_none());
                        assert_eq!(
                            runtime
                                .publish_plugin_transition(record, &fence, None)
                                .await
                                .unwrap_err()
                                .code,
                            crate::RuntimeErrorCode::StoreCommitFailed
                        );
                        support::assert_unpublished(&store, &initial).await;
                        assert!(runtime.session.is_none());
                    }
                    Verdict::Stale => {
                        seal_shift_fence_for_test(&raw, &initial.session_id, "successor").await;
                        let error = runtime
                            .publish_plugin_transition(record, &fence, None)
                            .await
                            .unwrap_err();
                        assert_eq!(error.code, crate::RuntimeErrorCode::StoreCommitSuperseded);
                        support::assert_unpublished(&store, &initial).await;
                        assert!(runtime.session.is_none());
                    }
                    Verdict::Publish => {
                        let mut expected = record.candidate().unwrap();
                        let target = record.request.target.clone();
                        let resume = resume_head.lock_recover().clone();
                        if resume.is_some() && cut != Cut::PublishedThenFreshShift {
                            let inactive = expected.0.plugins.get_mut("inactive").unwrap();
                            inactive.generation = 10;
                            inactive
                                .values
                                .insert("retained".into(), serde_json::json!(99));
                        }
                        runtime
                            .publish_plugin_transition(record, &fence, resume.as_ref())
                            .await
                            .unwrap();
                        if cut == Cut::PublishedThenFreshShift && resume.is_some() {
                            seal_shift_fence_for_test(&raw, &initial.session_id, "successor").await;
                            assert_eq!(
                                record_resuming(
                                    &controller,
                                    host,
                                    store.clone(),
                                    initial.clone(),
                                    request,
                                    resume.clone().map(|head| (head, fence.clone())),
                                )
                                .await
                                .unwrap_err()
                                .into_runtime_error()
                                .code,
                                crate::RuntimeErrorCode::StoreCommitSuperseded,
                                "resuming a published transition still refuses a superseded fence"
                            );
                        }
                        assert!(
                            runtime.session.is_some(),
                            "production publication constructs the session"
                        );
                        assert_eq!(runtime.services.plugins.plugin_admission(), Some(target));
                        assert_eq!(runtime.services.plugins.export_state(), expected.0);
                        assert_eq!(
                            *runtime.services.plugins.admitted_plugin_config().config,
                            expected.1
                        );
                        let head = store.load_session_head_meta().await.unwrap().unwrap();
                        assert_eq!(runtime.state.head_revision, head.head_revision);
                        assert_eq!(runtime.state.checkpoint_ref, head.checkpoint_ref);
                        assert_eq!(
                            head.head_revision,
                            initial.head_revision + if resume.is_some() { 2 } else { 1 },
                            "one atomic publication despite lost replies"
                        );
                        let mut retained = published_head.lock_recover();
                        if let Some(previous) = retained.as_ref() {
                            assert_eq!(
                                &(head.head_revision, head.checkpoint_ref, head.leaf_node_id),
                                previous,
                                "a replay reuses the committed receipt"
                            );
                        } else {
                            *retained =
                                Some((head.head_revision, head.checkpoint_ref, head.leaf_node_id));
                        }
                    }
                }
                assert_eq!(counts.callbacks.load(Ordering::SeqCst), 0);
                assert_eq!(counts.providers.load(Ordering::SeqCst), 0);
                if first
                    && matches!(
                        cut,
                        Cut::PublishedThenHeadAdvanced | Cut::PublishedThenFreshShift
                    )
                {
                    let advanced = support::advance_inactive_namespace(&raw, &store, &fence).await;
                    *published_head.lock_recover() = Some((
                        advanced.revision,
                        advanced.checkpoint.clone(),
                        advanced.leaf.clone(),
                    ));
                    *resume_head.lock_recover() = Some(advanced);
                    if cut == Cut::PublishedThenHeadAdvanced {
                        panic!(
                            "the transition committed and a later head advanced before cold adoption"
                        );
                    }
                }
                if first && cut == Cut::PublishedBeforeReply {
                    panic!("publication decision before reply");
                }
            })
        })
    };
    let admitted = crate::AdmittedScope::turn(&id, "run");
    match cut {
        Cut::PublishedThenFreshShift => {
            double
                .run_in_handler(admitted.clone(), attempt.clone())
                .await
                .unwrap();
            double.run_in_handler(admitted, attempt).await.unwrap();
        }
        Cut::ConvertedBeforeRecord | Cut::RecordedBeforeAck => {
            let point = if cut == Cut::ConvertedBeforeRecord {
                lash_restate_test::CrashPoint::BeforeRunResult { name: None }
            } else {
                lash_restate_test::CrashPoint::BeforeFrame {
                    ty: lash_restate_test::protocol::MessageType::ProposeRunCompletionAck,
                }
            };
            double
                .server()
                .crash_on(lash_restate_test::CrashRule::new(point));
            double.run_in_handler(admitted, attempt).await.unwrap();
        }
        _ => double
            .run_crashed_then_redriven(admitted, attempt.clone(), attempt)
            .await
            .unwrap(),
    }
    assert_eq!(
        attempts.load(Ordering::SeqCst),
        2,
        "{verdict:?} at {cut:?}: both attempts executed"
    );
    let computations = if cut == Cut::ConvertedBeforeRecord {
        2
    } else {
        1
    };
    assert_eq!(
        counts.converters.load(Ordering::SeqCst),
        computations * 2,
        "one state and one config conversion per unrecorded attempt"
    );
    assert_eq!(counts.initializers.load(Ordering::SeqCst), computations);
    let constructions = if verdict != Verdict::Publish {
        0
    } else if matches!(
        cut,
        Cut::PublishedBeforeReply | Cut::PublishedThenHeadAdvanced | Cut::PublishedThenFreshShift
    ) {
        4
    } else {
        2
    };
    assert_eq!(counts.builds.load(Ordering::SeqCst), constructions);
    assert_eq!(counts.registers.load(Ordering::SeqCst), constructions);
    assert_eq!(counts.ready.load(Ordering::SeqCst), constructions);
    assert_eq!(counts.callbacks.load(Ordering::SeqCst), 0);
    assert_eq!(counts.providers.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn production_publication_crash_matrix_sqlite_memory() {
    matrix(Stores::Memory(Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory().await.unwrap(),
    )))
    .await;
}

#[tokio::test]
async fn production_publication_crash_matrix_sqlite_file_reopen() {
    let files = tempfile::tempdir().unwrap();
    matrix(Stores::File(files.path().to_owned())).await;
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run in a private pg16 gate"]
async fn production_publication_crash_matrix_postgres() {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::PostgresStorage::connect(database.url())
        .await
        .unwrap();
    let files = tempfile::tempdir().unwrap();
    matrix(Stores::Memory(Arc::new(
        lash_postgres_store::PostgresStoreSet::new(
            &storage,
            Arc::new(crate::facade_support::FileAttachmentStore::new(
                files.path().to_owned(),
            )),
        ),
    )))
    .await;
}
