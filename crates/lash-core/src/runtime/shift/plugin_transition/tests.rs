//! Raw-store atomicity and receipt laws. Production publication/adoption is in `publication`.
use super::*;
use crate::testing::store_fixtures::{root_session_request, seal_shift_fence_for_test};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
struct Initializer(Arc<AtomicUsize>);
impl crate::PluginFactory for Initializer {
    fn id(&self) -> &'static str {
        "publish-probe"
    }
    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial(self.id())
    }
    fn initialize_state(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::PluginConfig,
    ) -> Result<BTreeMap<String, serde_json::Value>, crate::PluginError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(BTreeMap::from([(
            "initialized".into(),
            serde_json::json!(17),
        )]))
    }
    fn build(
        &self,
        _: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        panic!("publication never constructs capabilities")
    }
}

#[derive(Clone)]
struct Converter(Arc<AtomicUsize>, bool);
impl crate::PluginFactory for Converter {
    fn id(&self) -> &'static str {
        "convert-probe"
    }
    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        let mut declaration = crate::plugin::PluginDeclaration::initial(self.id());
        declaration.format_version = crate::FormatVersion::new(2).unwrap();
        declaration.writable_formats = vec![crate::FormatVersion::ONE, declaration.format_version];
        declaration
    }
    fn migrate_format(
        &self,
        from: crate::FormatVersion,
        namespace: crate::FormatNamespace,
        mut value: serde_json::Value,
    ) -> Result<serde_json::Value, crate::FormatRefusal> {
        assert_eq!(
            from,
            crate::FormatVersion::ONE,
            "only the recorded old namespace converts"
        );
        self.0.fetch_add(1, Ordering::SeqCst);
        let object = value.as_object_mut().unwrap();
        let old = object.remove("old").unwrap();
        object.insert("native".into(), old);
        if self.1 && namespace == crate::FormatNamespace::State {
            return Err(crate::FormatRefusal {
                plugin: self.id().into(),
                namespace,
                stored: from,
                readable: self.declaration().format_version,
            });
        }
        Ok(value)
    }
    fn encode_format(
        &self,
        to: crate::FormatVersion,
        _: crate::FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::FormatRefusal> {
        let mut value = value.clone();
        if to == crate::FormatVersion::ONE {
            let object = value.as_object_mut().unwrap();
            let native = object.remove("native").unwrap();
            object.insert("old".into(), native);
        }
        Ok(value)
    }
    fn build(
        &self,
        _: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        panic!("publication never constructs capabilities")
    }
}

async fn record(
    controller: &crate::ScopedEffectController<'_>,
    host: crate::PluginHost,
    store: crate::store::SessionStore,
    initial: crate::RuntimeSessionState,
    request: crate::plugin::PluginTransitionRequest,
) -> crate::plugin::PluginTransitionRecord {
    record_resuming(controller, host, store, initial, request, None)
        .await
        .unwrap()
}

async fn record_resuming(
    controller: &crate::ScopedEffectController<'_>,
    host: crate::PluginHost,
    store: crate::store::SessionStore,
    initial: crate::RuntimeSessionState,
    request: crate::plugin::PluginTransitionRequest,
    resume: Option<(crate::store::SessionHeadRef, crate::store::ShiftFence)>,
) -> Result<crate::plugin::PluginTransitionRecord, crate::RuntimeEffectControllerError> {
    let invocation = crate::RuntimeEffectInvocation::new(
        request.id.0.clone(),
        crate::RuntimeAttribution::for_session(initial.session_id.clone()),
        "plugin-transition",
    );
    let outcome = controller
        .execute_effect(
            crate::RuntimeEffectEnvelope::new(
                invocation,
                crate::RuntimeEffectCommand::TransitionPlugins {
                    request: Box::new(request),
                },
            ),
            lash_core_execution::core_internal::owned_runner_executor(
                Box::new(PluginTransitionRunner {
                    host,
                    store,
                    initial,
                    raw_plugins: Default::default(),
                    commit_budget: crate::testing::runtime_helpers::test_commit_budget(),
                    resume,
                }),
                None,
            ),
        )
        .await?;
    let crate::RuntimeEffectOutcome::TransitionPlugins { record } = outcome else {
        panic!("transition outcome")
    };
    Ok(*record)
}

async fn matrix(file: bool, stale: bool, refuse: bool) {
    let files = tempfile::tempdir().unwrap();
    let stores: Arc<dyn crate::StoreSet> = if file {
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::open(files.path())
                .await
                .unwrap(),
        )
    } else {
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap())
    };
    let double = lash_restate_test::backend_with(
        0x4857_0002,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .unwrap();
    let backend = double.lash_backend();
    for boundary in 0..3 {
        let id = crate::SessionId::fixture(format!("publication-{file}-{stale}-{boundary}"));
        let factory = backend.session_store_factory();
        factory
            .admit_session(&root_session_request(&id))
            .await
            .unwrap();
        let raw: Arc<dyn crate::RuntimeStore> = factory;
        let store = crate::store::SessionStore::new(raw.clone(), id.clone()).unwrap();
        let loaded =
            crate::store::load_session_window_state(&store, crate::store::WindowSelector::Current)
                .await
                .unwrap()
                .unwrap();
        let mut initial = loaded.state;
        let conversions = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let host = crate::PluginHost::new(vec![
            Arc::new(Initializer(calls.clone())),
            Arc::new(Converter(conversions.clone(), refuse)),
        ]);
        let target = host.admit_plugins(raw.as_ref()).await.unwrap();
        initial.set_plugin_state(Some(crate::PluginState {
            plugins: BTreeMap::from([(
                "convert-probe".into(),
                crate::PluginNamespaceState {
                    format_version: crate::FormatVersion::ONE,
                    generation: 7,
                    values: BTreeMap::from([("old".into(), serde_json::json!(17))]),
                },
            )]),
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
        let old_head = store.load_session_head_meta().await.unwrap().unwrap();
        let scope = crate::ExecutionScope::turn(&id, "run");
        let request = crate::plugin::PluginTransitionRequest {
            id: crate::plugin::PluginTransitionId(
                crate::EffectAddress::new(scope, "plugin-transition").unwrap(),
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
        let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
            let host = host.clone();
            let initial = initial.clone();
            let request = request.clone();
            let store = store.clone();
            let raw = raw.clone();
            let id = id.clone();
            let fence = fence.clone();
            let old_head = old_head.clone();
            Arc::new(move |controller| {
                let host = host.clone();
                let initial = initial.clone();
                let request = request.clone();
                let store = store.clone();
                let raw = raw.clone();
                let id = id.clone();
                let fence = fence.clone();
                let old_head = old_head.clone();
                Box::pin(async move {
                    if crash && boundary == 0 {
                        panic!("before recording");
                    }
                    let record = record(
                        &controller,
                        host.isolated_registry(),
                        store.clone(),
                        initial,
                        request,
                    )
                    .await;
                    if refuse {
                        assert!(matches!(
                            record.candidate(),
                            Err(crate::PluginError::Format(crate::FormatRefusal {
                                namespace: crate::FormatNamespace::State,
                                ..
                            }))
                        ));
                        assert!(
                            record.publication.is_none(),
                            "one refused namespace makes the entire candidate unpublishable"
                        );
                        let current = crate::store::load_session_window_state(
                            &store,
                            crate::store::WindowSelector::Current,
                        )
                        .await
                        .unwrap()
                        .unwrap()
                        .state;
                        assert_eq!(current.head_revision, old_head.head_revision);
                        assert!(current.plugin_admission_snapshot().is_none());
                        assert!(
                            !current
                                .plugin_state()
                                .unwrap()
                                .plugins
                                .contains_key("publish-probe")
                        );
                        assert_eq!(
                            current.plugin_state().unwrap().plugins["convert-probe"].values["old"],
                            serde_json::json!(17)
                        );
                        assert_eq!(
                            current
                                .authority
                                .plugin_config
                                .namespace("convert-probe")
                                .unwrap()
                                .format_version,
                            crate::FormatVersion::ONE
                        );
                        if crash {
                            panic!("typed refusal recorded without publication");
                        }
                        return;
                    }
                    record.candidate().unwrap();
                    if crash && boundary == 1 {
                        assert_eq!(
                            store
                                .load_session_head_meta()
                                .await
                                .unwrap()
                                .unwrap()
                                .head_revision,
                            old_head.head_revision
                        );
                        let current = crate::store::load_session_window_state(
                            &store,
                            crate::store::WindowSelector::Current,
                        )
                        .await
                        .unwrap()
                        .unwrap()
                        .state;
                        assert!(current.plugin_admission_snapshot().is_none());
                        panic!("recorded result before publication");
                    }
                    let mut commit = *record.publication.unwrap();
                    commit.shift_fence = Some(Box::new(fence));
                    if stale {
                        seal_shift_fence_for_test(&raw, &id, "successor").await;
                        assert!(matches!(
                            raw.commit_runtime_state(commit).await,
                            Err(crate::StoreError::StaleShiftFence { .. })
                        ));
                        assert_eq!(
                            store
                                .load_session_head_meta()
                                .await
                                .unwrap()
                                .unwrap()
                                .head_revision,
                            old_head.head_revision
                        );
                        assert!(
                            crate::store::load_session_window_state(
                                &store,
                                crate::store::WindowSelector::Current
                            )
                            .await
                            .unwrap()
                            .unwrap()
                            .state
                            .plugin_admission_snapshot()
                            .is_none()
                        );
                        if crash {
                            panic!("stale owner refused");
                        }
                        return;
                    }
                    let receipt = raw.commit_runtime_state(commit.clone()).await.unwrap();
                    let current = crate::store::load_session_window_state(
                        &store,
                        crate::store::WindowSelector::Current,
                    )
                    .await
                    .unwrap()
                    .unwrap()
                    .state;
                    let native = crate::plugin::PluginNativeView::decode(
                        &current.plugin_admission_snapshot().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        native.state.plugins["publish-probe"].values["initialized"],
                        serde_json::json!(17)
                    );
                    assert_eq!(native.request.target, record.request.target);
                    assert_eq!(
                        native.state.plugins["convert-probe"].format_version.get(),
                        2
                    );
                    assert_eq!(
                        native.state.plugins["convert-probe"].values["native"],
                        serde_json::json!(17)
                    );
                    let writer = native.request.target.writer("convert-probe").unwrap();
                    let written = &current.plugin_state().unwrap().plugins["convert-probe"];
                    assert_eq!(written.format_version, writer);
                    let key = if writer.get() == 1 { "old" } else { "native" };
                    assert_eq!(written.values[key], serde_json::json!(17));
                    assert_eq!(
                        current
                            .authority
                            .plugin_config
                            .namespace("convert-probe")
                            .unwrap()
                            .format_version,
                        writer
                    );
                    assert_eq!(current.head_revision, receipt.head_revision);
                    if crash {
                        panic!("publication committed before acknowledgement");
                    }
                    if boundary == 2 {
                        assert!(receipt.receipt_replayed);
                    }
                    let replay = raw.commit_runtime_state(commit).await.unwrap();
                    assert!(replay.receipt_replayed);
                    assert_eq!(replay.head_revision, receipt.head_revision);
                })
            })
        };
        double
            .run_crashed_then_redriven(
                crate::AdmittedScope::turn(&id, "run"),
                attempt(true),
                attempt(false),
            )
            .await
            .unwrap();
        assert_eq!(
            conversions.load(Ordering::SeqCst),
            2,
            "cold replay serves both state and config conversion results without invoking either converter"
        );
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "cold replay never re-initializes a recorded transition"
        );
    }
}

#[tokio::test]
async fn transition_publish_crash_matrix_sqlite_memory() {
    matrix(false, false, false).await;
}
#[tokio::test]
async fn transition_publish_crash_matrix_sqlite_file() {
    matrix(true, false, false).await;
}
#[tokio::test]
async fn stale_owner_cannot_publish_transition_sqlite_memory() {
    matrix(false, true, false).await;
}
#[tokio::test]
async fn stale_owner_cannot_publish_transition_sqlite_file() {
    matrix(true, true, false).await;
}

#[tokio::test]
async fn one_refused_namespace_publishes_neither_sqlite_memory() {
    matrix(false, false, true).await;
}
#[tokio::test]
async fn one_refused_namespace_publishes_neither_sqlite_file() {
    matrix(true, false, true).await;
}

mod publication;
