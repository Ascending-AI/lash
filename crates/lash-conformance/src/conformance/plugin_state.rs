//! ADR 0078 laws: the same plugin and runtime checkpoint path on every backend.
use super::*;
use crate::plugin::{
    PluginFactory, PluginRegistrar, PluginSessionContext, RecordedSessionConfig, SessionPlugin,
    SessionReadyContext,
};
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
            .insert(state.session_id().into(), state.clone());
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
        self.ready_values.lock_recover().insert(
            context.session_id.clone().to_string(),
            context.state.get("counter"),
        );
        let registered = self.handles.lock_recover()[context.session_id.as_str()].clone();
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

async fn commit(store: &Arc<dyn RuntimePersistence>, state: &mut RuntimeSessionState) {
    let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
        store,
        RuntimeCommit::persisted_state_for_test(state, &[]),
        "plugin-state-law",
    )
    .await
    .expect("boundary commit");
    state.apply_persisted_commit_result(receipt);
}

pub async fn plugin_state_boundary(
    make: impl Fn(&str) -> Arc<dyn RuntimePersistence>,
    label: &str,
) {
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
pub async fn plugin_state_boundary_trace(
    store: Arc<dyn RuntimePersistence>,
    parent_id: &str,
    child_store: Arc<dyn RuntimePersistence>,
    child_id: &str,
) -> Vec<lash_core::PluginState> {
    let fixture = MockPlugin::default();
    let host = fixture.host();
    let plugins = host.build_session(parent_id).expect("build");
    let handle = fixture.state(parent_id);
    let mut state = RuntimeSessionState {
        session_id: parent_id.into(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.refresh_plugin_states(&plugins);
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
    let crash_state = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .unwrap()
        .unwrap();
    let rebuilt_fixture = MockPlugin::default();
    let rebuilt_host = rebuilt_fixture.host();
    let rebuilt = rebuilt_host
        .rematerialize_session(
            parent_id,
            crash_state.plugin_state().unwrap(),
            RecordedSessionConfig::new(Default::default()),
        )
        .unwrap();
    assert_eq!(
        rebuilt_fixture.state(parent_id).get("counter"),
        None,
        "uncommitted tail is lost on rebuild"
    );
    drop(rebuilt);
    state.refresh_plugin_states(&plugins);
    let changed = RuntimeCommit::persisted_state_for_test(&state, &[]);
    assert!(
        matches!(
            changed.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
            crate::HydratedCheckpointComponent::Changed { .. }
        ),
        "generation moved: checkpoint must be Changed"
    );
    commit(&store, &mut state).await;
    assert_ne!(state.plugin_state_ref(), before.as_ref());
    state.refresh_plugin_states(&plugins);
    let unchanged = RuntimeCommit::persisted_state_for_test(&state, &[]);
    assert!(
        matches!(
            unchanged.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
            crate::HydratedCheckpointComponent::Unchanged { .. }
        ),
        "generation unchanged: checkpoint must use its resident reference"
    );
    let durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .unwrap()
        .unwrap();
    let rebuilt = rebuilt_host
        .rematerialize_session(
            parent_id,
            durable.plugin_state().unwrap(),
            RecordedSessionConfig::new(Default::default()),
        )
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    child_state.refresh_plugin_states(&child);
    commit(&child_store, &mut child_state).await;
    let child_durable = crate::store::load_persisted_session_state(child_store.as_ref())
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
async fn runtime_plugin_state_park_law(store: Arc<dyn RuntimePersistence>) {
    let id = "plugin-state-lifecycle";
    let fixture = MockPlugin {
        writes_on_ready: true,
        ..Default::default()
    };
    let policy = crate::SessionPolicy {
        model: crate::ModelSpec::builder("plugin-state-model")
            .context_window_tokens(4096)
            .build()
            .unwrap(),
        ..crate::SessionPolicy::new(crate::TurnBudget::Unbounded)
    };
    let state = RuntimeSessionState {
        session_id: id.into(),
        ..RuntimeSessionState::new(policy.clone())
    };
    let plugins = fixture.host().build_session(id).unwrap();
    let hook_session = plugins.clone();
    let runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy.clone(),
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )),
        crate::PersistentRuntimeServices::new(plugins, store.clone()),
        state,
        crate::testing::runtime_lease_owner(),
    )
    .await
    .unwrap();
    let hook_error = hook_session
        .before_turn(crate::plugin::TurnHookContext {
            session_id: id.into(),
            state: runtime.read_view().expect("runtime frame scope resolves"),
            sessions: runtime.session_state_service().unwrap(),
            turn_context: crate::TurnContext::default(),
        })
        .await
        .expect_err("hook deliberately fails after its accepted write");
    assert!(hook_error.to_string().contains("deliberate hook failure"));
    fixture
        .state(id)
        .set("counter", serde_json::json!(11))
        .unwrap();
    runtime.park().await.unwrap();
    let state = crate::store::load_persisted_session_state(store.as_ref())
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
        .rematerialize_session(
            id,
            durable,
            RecordedSessionConfig::new(state.protocol_turn_options.clone()),
        )
        .unwrap();
    assert_eq!(rebuilt.state(id).generation(), generation + 1);
    let runtime = crate::LashRuntime::from_persistent_embedded_state(
        policy,
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )),
        crate::PersistentRuntimeServices::new(plugins, store.clone()),
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
    runtime.park().await.unwrap();
    let final_state = crate::store::load_persisted_session_state(store.as_ref())
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
async fn registration_state_law(
    store: Arc<dyn RuntimePersistence>,
    id: &str,
    registration: Registration,
) {
    let fixture = MockPlugin::default();
    let plugins = fixture.host().build_session(id).unwrap();
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
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.refresh_plugin_states(&plugins);
    commit(&store, &mut state).await;
    drop(plugins);
    let mut durable = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .unwrap()
        .unwrap();
    let rebuilt = MockPlugin {
        registration,
        ..Default::default()
    };
    let plugins = rebuilt
        .host()
        .rematerialize_session(
            id,
            durable.plugin_state().unwrap(),
            RecordedSessionConfig::new(Default::default()),
        )
        .unwrap();
    assert_eq!(rebuilt.state(id).generation(), 6);
    durable.refresh_plugin_states(&plugins);
    commit(&store, &mut durable).await;
    let final_state = crate::store::load_persisted_session_state(store.as_ref())
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
