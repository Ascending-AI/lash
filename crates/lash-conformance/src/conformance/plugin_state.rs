//! ADR 0078 laws: the same plugin and runtime checkpoint path on every backend.
use super::*;
use crate::plugin::{
    PluginFactory, PluginRegistrar, PluginSessionContext, SessionAuthorityContext, SessionPlugin,
    SessionReadyContext,
};
use lash_core::plugin::PluginSessionRequest;
use lash_core::{PluginError, PluginStateEdit, PluginStateError, PluginStateStore};
use lash_sansio::sync::MutexExt;
use pretty_assertions::assert_eq;
use std::sync::Mutex;

#[derive(Clone, Copy, Default)]
enum Registration {
    #[default]
    None,
    Remove,
    Admission,
}

#[derive(Clone, Default)]
struct MockPlugin {
    writes_on_ready: bool,
    registration: Registration,
    ready_values: Arc<Mutex<std::collections::BTreeMap<String, Option<serde_json::Value>>>>,
    handles: Arc<Mutex<std::collections::BTreeMap<String, PluginStateStore>>>,
}
impl PluginFactory for MockPlugin {
    fn id(&self) -> &'static str {
        "mock-state"
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(PluginFactory::id(self))
    }
    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}
impl SessionPlugin for MockPlugin {
    fn id(&self) -> &'static str {
        "mock-state"
    }
    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        let state = registrar.state();
        match self.registration {
            Registration::None => {}
            Registration::Remove => {
                assert_eq!(state.remove("counter")?, 6);
                assert_eq!(state.remove("absent")?, 6);
                assert_eq!(state.get("counter"), None);
            }
            Registration::Admission => {
                assert!(
                    matches!(
                        state.set("overflow", serde_json::json!("x".repeat(32766))),
                        Err(PluginStateError::StoreTooLarge { .. })
                    ),
                    "oversize register write must be rejected at registration"
                );
                assert_eq!(state.generation(), 5);
                assert_eq!(state.get("overflow"), None);
                assert_eq!(
                    state.set("accepted", serde_json::json!("x".repeat(1024)))?,
                    6
                );
            }
        }
        self.handles
            .lock_recover()
            .insert(owner_key(state.owner()), state.clone());
        if self.writes_on_ready {
            registrar.turn().before(Arc::new(move |_| {
                let state = state.clone();
                Box::pin(async move {
                    state.set("failed-hook", serde_json::json!(true))?;
                    Err(PluginError::Session(
                        "deliberate hook failure after accepted write".into(),
                    ))
                })
            }));
        }
        Ok(())
    }
    fn session_ready(&self, context: SessionReadyContext) -> Result<(), PluginError> {
        self.ready_values
            .lock_recover()
            .insert(owner_key(&context.owner), context.state.get("counter"));
        let registered = self.handles.lock_recover()[&owner_key(&context.owner)].clone();
        assert_eq!(registered.generation(), context.state.generation());
        assert_eq!(
            registered.get("counter"),
            context.state.get("counter"),
            "ready must observe hydrated state through the captured registrar handle"
        );
        if self.writes_on_ready {
            context.state.set("ready", serde_json::json!(true))?;
        }
        Ok(())
    }
}
impl MockPlugin {
    fn host(&self) -> crate::PluginHost {
        let mut factories = crate::testing::test_standard_protocol_factories();
        factories.push(Arc::new(self.clone()));
        crate::PluginHost::new(factories)
    }
    fn state(&self, id: &str) -> PluginStateStore {
        self.handles.lock_recover()[id].clone()
    }
}

/// The fixture's key for a plugin session: the session id the laws look it up
/// by, or the owner's own spelling for a process.
fn owner_key(owner: &crate::RuntimeOwner) -> String {
    match owner {
        crate::RuntimeOwner::Session(session_id) => session_id.to_string(),
        crate::RuntimeOwner::Process(_) => owner.to_string(),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn commit(store: &Arc<dyn RuntimeStore>, state: &mut RuntimeSessionState) {
    let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
        store,
        RuntimeCommit::persisted_state_for_test(state),
        "plugin-state-law",
    )
    .await
    .expect("boundary commit");
    state.apply_persisted_commit_result(receipt);
}

pub async fn plugin_state_boundary(make: impl Fn(&str) -> Arc<dyn RuntimeStore>, label: &str) {
    for version in [1, 3] {
        let id = format!("{label}-format-{version}");
        plugin_format_boundary(make(&id), &id, version).await;
    }
    let register_remove = format!("{label}-register-remove");
    registration_state_law(
        make(&register_remove),
        &register_remove,
        Registration::Remove,
    )
    .await;
    let register_admission = format!("{label}-register-admission");
    registration_state_law(
        make(&register_admission),
        &register_admission,
        Registration::Admission,
    )
    .await;
    let parent = format!("{label}-parent");
    let child = format!("{label}-child");
    plugin_state_boundary_trace(make(&parent), &parent, make(&child), &child).await;
    let lifecycle = format!("{label}-lifecycle");
    Box::pin(runtime_plugin_state_park_law(make(&lifecycle))).await;
}

/// Execute the plugin-state boundary and fork laws, returning decoded checkpoint
/// bodies for independent cross-backend comparison.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn plugin_state_boundary_trace(
    store: Arc<dyn RuntimeStore>,
    parent_id: &str,
    child_store: Arc<dyn RuntimeStore>,
    child_id: &str,
) -> Vec<lash_core::PluginState> {
    let fixture = MockPlugin::default();
    let host = fixture.host();
    let plugins = host
        .build_session(PluginSessionRequest::creation(
            parent_id,
            Default::default(),
        ))
        .expect("build");
    let handle = fixture.state(parent_id);
    let mut state = RuntimeSessionState {
        session_id: parent_id.into(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state
        .refresh_plugin_states(&plugins)
        .expect("the live plugin state is captured");
    commit(&store, &mut state).await;
    let before = state.plugin_state_ref().cloned();
    assert_eq!(handle.set("counter", serde_json::json!(1)).unwrap(), 1);
    assert_eq!(
        handle.clone().get("counter"),
        Some(serde_json::json!(1)),
        "read your writes"
    );
    let rejected = handle.apply_guarded(
        0,
        vec![PluginStateEdit::Set {
            key: "counter".into(),
            value: serde_json::json!(99),
        }],
    );
    assert!(matches!(
        rejected,
        Err(PluginStateError::GenerationConflict {
            expected: 0,
            actual: 1
        })
    ));
    assert_eq!(handle.get("counter"), Some(serde_json::json!(1)));
    let crash_state =
        crate::conformance::helpers::load_window_state(&store, &SessionId::from(parent_id))
            .await
            .unwrap()
            .unwrap();
    let rebuilt_fixture = MockPlugin::default();
    let rebuilt_host = rebuilt_fixture.host();
    let rebuilt = rebuilt_host
        .build_session(PluginSessionRequest::rematerialization(
            parent_id,
            crash_state.plugin_state().unwrap(),
            SessionAuthorityContext::default(),
        ))
        .unwrap();
    assert_eq!(
        rebuilt_fixture.state(parent_id).get("counter"),
        None,
        "uncommitted tail is lost on rebuild"
    );
    drop(rebuilt);
    state
        .refresh_plugin_states(&plugins)
        .expect("the live plugin state is captured");
    let changed = RuntimeCommit::persisted_state_for_test(&state);
    assert!(
        matches!(
            changed.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
            crate::HydratedCheckpointComponent::Changed { .. }
        ),
        "generation moved: checkpoint must be Changed"
    );
    commit(&store, &mut state).await;
    assert_ne!(state.plugin_state_ref(), before.as_ref());
    state
        .refresh_plugin_states(&plugins)
        .expect("the live plugin state is captured");
    let unchanged = RuntimeCommit::persisted_state_for_test(&state);
    assert!(
        matches!(
            unchanged.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
            crate::HydratedCheckpointComponent::Unchanged { .. }
        ),
        "generation unchanged: checkpoint must use its resident reference"
    );
    let durable =
        crate::conformance::helpers::load_window_state(&store, &SessionId::from(parent_id))
            .await
            .unwrap()
            .unwrap();
    let rebuilt = rebuilt_host
        .build_session(PluginSessionRequest::rematerialization(
            parent_id,
            durable.plugin_state().unwrap(),
            SessionAuthorityContext::default(),
        ))
        .unwrap();
    assert_eq!(
        rebuilt_fixture.state(parent_id).get("counter"),
        Some(serde_json::json!(1)),
        "committed write survives process reconstruction"
    );
    assert_eq!(
        rebuilt_fixture.ready_values.lock_recover()[parent_id],
        Some(serde_json::json!(1)),
        "session_ready itself must see the committed value"
    );
    assert_eq!(rebuilt.export_state(), plugins.export_state());
    handle.set("counter", serde_json::json!(2)).unwrap();
    let child = plugins
        .fork_for_session(child_id, Default::default())
        .unwrap();
    let child_handle = fixture.state(child_id);
    assert_eq!(child_handle.generation(), handle.generation());
    assert_eq!(
        child_handle.get("counter"),
        Some(serde_json::json!(2)),
        "fork includes uncommitted live parent state"
    );
    handle.set("counter", serde_json::json!(3)).unwrap();
    child_handle
        .set("child-only", serde_json::json!(true))
        .unwrap();
    assert_eq!(child_handle.get("counter"), Some(serde_json::json!(2)));
    assert_eq!(handle.get("child-only"), None);
    let mut child_state = RuntimeSessionState {
        session_id: child_id.into(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    child_state
        .refresh_plugin_states(&child)
        .expect("the live plugin state is captured");
    commit(&child_store, &mut child_state).await;
    let child_durable =
        crate::conformance::helpers::load_window_state(&child_store, &SessionId::from(child_id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(child_durable.plugin_state(), Some(&child.export_state()));
    vec![
        crash_state.plugin_state().unwrap().clone(),
        durable.plugin_state().unwrap().clone(),
        child_durable.plugin_state().unwrap().clone(),
    ]
}

// Exercise production construction and park; this witness never explicitly
// refreshes components or manufactures a checkpoint for the runtime.
#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn runtime_plugin_state_park_law(store: Arc<dyn RuntimeStore>) {
    let id = "plugin-state-lifecycle";
    let fixture = MockPlugin {
        writes_on_ready: true,
        ..Default::default()
    };
    let policy = crate::SessionPolicy {
        model: Some(crate::testing::test_llm_profile_config(
            "plugin-state-model",
            crate::LlmProfileMetadata::builder("plugin-state-model")
                .context_window_tokens(4096)
                .build()
                .unwrap(),
        )),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024))
    };
    let state = RuntimeSessionState {
        session_id: id.into(),
        ..RuntimeSessionState::new(policy.clone())
    };
    let plugins = fixture
        .host()
        .build_session(PluginSessionRequest::creation(id, Default::default()))
        .unwrap();
    let hook_session = plugins.clone();
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::StoreLawBackend::new().host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        crate::conformance::helpers::session_view(&store, id),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy.clone(),
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .unwrap();
    let hook_error = hook_session
        .before_turn(crate::plugin::TurnHookContext {
            session_id: id.into(),
            state: runtime.read_view(),
            sessions: runtime.session_state_service().unwrap(),
            turn_context: crate::TurnContext::default(),
            plugin_config: Default::default(),
        })
        .await
        .expect_err("hook deliberately fails after its accepted write");
    assert!(hook_error.to_string().contains("deliberate hook failure"));
    fixture
        .state(id)
        .set("counter", serde_json::json!(11))
        .unwrap();
    Box::pin(runtime.park()).await.unwrap();
    let state = crate::conformance::helpers::load_window_state(&store, &SessionId::from(id))
        .await
        .unwrap()
        .unwrap();
    let durable = state.plugin_state().unwrap();
    assert_eq!(
        durable.plugins["mock-state"].values["counter"],
        serde_json::json!(11)
    );
    assert_eq!(
        durable.plugins["mock-state"].values["ready"],
        serde_json::json!(true)
    );
    assert_eq!(
        durable.plugins["mock-state"].values["failed-hook"],
        serde_json::json!(true),
        "a hook error does not roll back accepted writes before the next boundary"
    );
    let generation = durable.plugins["mock-state"].generation;
    let rebuilt = MockPlugin {
        writes_on_ready: true,
        ..Default::default()
    };
    let plugins = rebuilt
        .host()
        .build_session(PluginSessionRequest::rematerialization(
            id,
            durable,
            SessionAuthorityContext {
                plugin_config: state.admitted_plugin_config(),
                ..Default::default()
            },
        ))
        .unwrap();
    assert_eq!(rebuilt.state(id).generation(), generation + 1);
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::StoreLawBackend::new().host_config(
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::PersistentRuntimeServices::new(
        plugins,
        crate::conformance::helpers::session_view(&store, id),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy,
        runtime_host,
        runtime_services,
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .unwrap();
    assert_eq!(
        rebuilt.state(id).generation(),
        generation + 1,
        "runtime assembly must preserve ready writes"
    );
    Box::pin(runtime.park()).await.unwrap();
    let final_state = crate::conformance::helpers::load_window_state(&store, &SessionId::from(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        final_state.plugin_state().unwrap().plugins["mock-state"].generation,
        generation + 1,
        "an otherwise idle park persists accepted ready writes"
    );
}

// Seed generation five durably, then exercise registration on a cold rebuild.
#[expect(
    clippy::unwrap_used,
    reason = "conformance-law fixture: the unwrap mirrors the setup above"
)]
async fn registration_state_law(
    store: Arc<dyn RuntimeStore>,
    id: &str,
    registration: Registration,
) {
    let fixture = MockPlugin::default();
    let plugins = fixture
        .host()
        .build_session(PluginSessionRequest::creation(id, Default::default()))
        .unwrap();
    let handle = fixture.state(id);
    handle.set("counter", serde_json::json!(true)).unwrap();
    for key in ["large-a", "large-b", "large-c"] {
        handle
            .set(key, serde_json::json!("x".repeat(32766)))
            .unwrap();
    }
    handle
        .set("padding", serde_json::json!("x".repeat(100)))
        .unwrap();
    assert_eq!(handle.generation(), 5);
    let mut state = RuntimeSessionState {
        session_id: id.into(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    state.refresh_plugin_states(&plugins).unwrap();
    commit(&store, &mut state).await;
    drop(plugins);
    let mut durable = crate::conformance::helpers::load_window_state(&store, &SessionId::from(id))
        .await
        .unwrap()
        .unwrap();
    let rebuilt = MockPlugin {
        registration,
        ..Default::default()
    };
    let plugins = rebuilt
        .host()
        .build_session(PluginSessionRequest::rematerialization(
            id,
            durable.plugin_state().unwrap(),
            SessionAuthorityContext::default(),
        ))
        .unwrap();
    assert_eq!(rebuilt.state(id).generation(), 6);
    durable.refresh_plugin_states(&plugins).unwrap();
    commit(&store, &mut durable).await;
    let final_state = crate::conformance::helpers::load_window_state(&store, &SessionId::from(id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(final_state.plugin_state(), Some(&plugins.export_state()));
    let namespace = &final_state.plugin_state().unwrap().plugins["mock-state"];
    assert_eq!(namespace.generation, 6);
    match registration {
        Registration::Remove => assert!(!namespace.values.contains_key("counter")),
        Registration::Admission => {
            assert!(!namespace.values.contains_key("overflow"));
            assert_eq!(
                namespace.values["accepted"],
                serde_json::json!("x".repeat(1024))
            );
        }
        Registration::None => unreachable!(),
    }
}

#[derive(Clone)]
struct FormatPlugin(Arc<std::sync::atomic::AtomicUsize>);

#[expect(
    clippy::unwrap_used,
    reason = "format law uses known versions and exact object fixtures"
)]
impl PluginFactory for FormatPlugin {
    fn id(&self) -> &'static str {
        "format-state"
    }
    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        let mut declaration = lash_core::plugin::PluginDeclaration::initial("format-state");
        declaration.format_version = lash_core::FormatVersion::new(2).unwrap();
        declaration.writable_formats =
            vec![lash_core::FormatVersion::ONE, declaration.format_version];
        declaration
    }
    fn migrate_format(
        &self,
        from: lash_core::FormatVersion,
        namespace: lash_core::FormatNamespace,
        mut value: serde_json::Value,
    ) -> Result<serde_json::Value, lash_core::FormatRefusal> {
        if from != lash_core::FormatVersion::ONE {
            return Err(lash_core::FormatRefusal {
                plugin: "format-state".into(),
                namespace,
                stored: from,
                readable: self.declaration().format_version,
            });
        }
        let map = value.as_object_mut().unwrap();
        let old = map.remove("count").unwrap();
        map.insert("total".into(), old);
        Ok(value)
    }
    fn encode_format(
        &self,
        to: lash_core::FormatVersion,
        namespace: lash_core::FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, lash_core::FormatRefusal> {
        let mut value = value.clone();
        if to == lash_core::FormatVersion::ONE {
            let map = value.as_object_mut().unwrap();
            let total = map.remove("total").unwrap();
            map.insert("count".into(), total);
        } else if to != self.declaration().format_version {
            return Err(lash_core::FormatRefusal {
                plugin: "format-state".into(),
                namespace,
                stored: to,
                readable: self.declaration().format_version,
            });
        }
        Ok(value)
    }
    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::new(self.clone()))
    }
}
impl SessionPlugin for FormatPlugin {
    fn id(&self) -> &'static str {
        "format-state"
    }
    fn register(&self, _: &mut PluginRegistrar) -> Result<(), PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn session_ready(&self, _: SessionReadyContext) -> Result<(), PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// A commit whose plugin namespaces are stamped outside the fleet record's
/// writer range is refused typed and leaves the session's head where it was
/// (FIG-4746).
#[expect(
    clippy::unwrap_used,
    reason = "format law asserts every persisted fixture and commit"
)]
async fn assert_plugin_write_refused(
    store: &Arc<dyn RuntimeStore>,
    state: &RuntimeSessionState,
    session_id: &str,
) {
    let head = |store: Arc<dyn RuntimeStore>| async move {
        store
            .load_session_head_meta(&SessionId::from(session_id))
            .await
            .unwrap()
            .map(|head| (head.head_revision, head.checkpoint_ref))
    };
    let before = head(Arc::clone(store)).await;
    let refused = crate::testing::store_fixtures::commit_runtime_state_for_test(
        store,
        RuntimeCommit::persisted_state_for_test(state),
        "plugin-state-law",
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(crate::StoreError::Incompatible {
                refusal: lash_core::compat::CompatRefusal::PluginWriterOutsideRange { .. }
            })
        ),
        "a plugin format outside the fleet's writer range must be refused: {refused:?}"
    );
    assert_eq!(head(Arc::clone(store)).await, before);
}

/// FIG-4745's refusal and migration at a real persisted checkpoint boundary.
#[expect(
    clippy::unwrap_used,
    reason = "format law asserts every persisted fixture and commit"
)]
async fn plugin_format_boundary(store: Arc<dyn RuntimeStore>, session_id: &str, version: u32) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(FormatPlugin(calls.clone())));
    let host = crate::PluginHost::new(factories);
    let mut state = RuntimeSessionState {
        session_id: session_id.into(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    // Include all bound namespaces so migration alone must dirty the component.
    let seed = host
        .build_session(PluginSessionRequest::creation(
            "format-seed",
            Default::default(),
        ))
        .unwrap();
    let mut snapshot = seed.export_state();
    snapshot.plugins.insert(
        "format-state".into(),
        lash_core::PluginNamespaceState {
            format_version: lash_core::FormatVersion::new(version).unwrap(),
            generation: 7,
            values: std::collections::BTreeMap::from([("count".into(), serde_json::json!(17))]),
        },
    );
    state.set_plugin_state(Some(snapshot));
    state.authority.plugin_config.insert_versioned(
        "format-state",
        lash_core::FormatVersion::new(version).unwrap(),
        serde_json::json!({"count": 17}),
    );
    // The fleet record permits what the plugin's registration provisions
    // (FIG-4746): the fixture's stored format stands for a build that wrote
    // it, so that build's registration is what the store is provisioned from.
    let newest = version.max(2);
    let permitted = store
        .provision_plugin_writers(&[crate::store::plugin_writers::PluginWriterRegistration {
            plugin: "format-state".into(),
            native: lash_core::FormatVersion::new(newest).unwrap(),
            writable: (1..=newest)
                .map(|format| lash_core::FormatVersion::new(format).unwrap())
                .collect(),
        }])
        .await
        .unwrap()
        .permitted_writer("format-state")
        .unwrap();
    if !permitted.contains(version) {
        // Inside a rollback window the fleet permits only the oldest format,
        // so no build can have published this one: the store refuses it and
        // publishes nothing.
        assert_plugin_write_refused(&store, &state, session_id).await;
        return;
    }
    commit(&store, &mut state).await;
    let mut durable =
        crate::conformance::helpers::load_window_state(&store, &SessionId::from(session_id))
            .await
            .unwrap()
            .unwrap();
    let original = durable.plugin_state().unwrap().clone();
    let bytes = rmp_serde::to_vec_named(&original).unwrap();
    let original_head = durable.head_revision;
    calls.store(0, std::sync::atomic::Ordering::SeqCst);
    let request = PluginSessionRequest::rematerialization(
        session_id,
        &original,
        SessionAuthorityContext {
            plugin_config: durable.admitted_plugin_config(),
            ..Default::default()
        },
    );
    let result = host.isolated_registry().build_session(request.clone());
    if version > 2 {
        assert!(matches!(result, Err(PluginError::Format(_))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let after =
            crate::conformance::helpers::load_window_state(&store, &SessionId::from(session_id))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(after.head_revision, original_head);
        assert_eq!(
            rmp_serde::to_vec_named(after.plugin_state().unwrap()).unwrap(),
            bytes
        );
        return;
    }
    let decoded = result.unwrap();
    let replay = host.isolated_registry().build_session(request).unwrap();
    assert_eq!(decoded.export_state(), replay.export_state());
    assert_eq!(
        decoded.admitted_plugin_config(),
        replay.admitted_plugin_config()
    );
    // Materialization publishes nothing.
    let before_commit =
        crate::conformance::helpers::load_window_state(&store, &SessionId::from(session_id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(before_commit.head_revision, original_head);
    assert_eq!(
        rmp_serde::to_vec_named(before_commit.plugin_state().unwrap()).unwrap(),
        bytes
    );
    durable.refresh_plugin_states(&decoded).unwrap();
    let pending = RuntimeCommit::persisted_state_for_test(&durable);
    assert!(matches!(
        pending.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
        crate::HydratedCheckpointComponent::Changed { .. }
    ));
    durable.authority.plugin_config = decoded.admitted_plugin_config().config.as_ref().clone();
    if !permitted.contains(2) {
        // Inside a rollback window the migrated namespace cannot be written
        // back in the plugin's native format.
        assert_plugin_write_refused(&store, &durable, session_id).await;
        // A session admitted inside the window writes the format its
        // admission chose from the fleet record (FIG-4747): the oldest one,
        // which the build beside it still reads. The commit lands, in the
        // stored shape.
        let admission = host.admit_plugins(store.as_ref()).await.unwrap();
        assert_eq!(admission.writer("format-state").unwrap().get(), 1);
        decoded.adopt_plugin_admission(admission);
        durable.refresh_plugin_states(&decoded).unwrap();
        commit(&store, &mut durable).await;
        let after =
            crate::conformance::helpers::load_window_state(&store, &SessionId::from(session_id))
                .await
                .unwrap()
                .unwrap();
        let namespace = &after.plugin_state().unwrap().plugins["format-state"];
        assert_eq!(namespace.format_version.get(), 1);
        assert_eq!(namespace.values["count"], serde_json::json!(17));
        let config = after
            .authority
            .plugin_config
            .namespace("format-state")
            .unwrap();
        assert_eq!(config.format_version.get(), 1);
        assert_eq!(config.value["count"], serde_json::json!(17));
        return;
    }
    // A finalized fleet admits the native format, and the admission chooses
    // it.
    let admission = host.admit_plugins(store.as_ref()).await.unwrap();
    assert_eq!(admission.writer("format-state").unwrap().get(), 2);
    decoded.adopt_plugin_admission(admission);
    durable.refresh_plugin_states(&decoded).unwrap();
    commit(&store, &mut durable).await;
    let after =
        crate::conformance::helpers::load_window_state(&store, &SessionId::from(session_id))
            .await
            .unwrap()
            .unwrap();
    let namespace = &after.plugin_state().unwrap().plugins["format-state"];
    assert_eq!(namespace.format_version.get(), 2);
    assert_eq!(namespace.generation, 8);
    assert_eq!(namespace.values["total"], serde_json::json!(17));
    assert_eq!(
        after
            .authority
            .plugin_config
            .namespace("format-state")
            .unwrap()
            .format_version
            .get(),
        2
    );
    assert_eq!(
        after.authority.plugin_config.get("format-state").unwrap()["total"],
        serde_json::json!(17)
    );
}
