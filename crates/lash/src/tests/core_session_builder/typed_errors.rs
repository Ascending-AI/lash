//! Typed causes survive the facade on the durable substrate (FIG-5307).

use super::*;
use lash_core::facade_support::{
    PluginRegistrar, PluginSessionContext, ReconfigureError, SessionPlugin,
};
use lash_core::plugin::SessionReadyContext;
use lash_core::{PluginError, PluginStateError};
use std::error::Error;

/// A refused tool-membership change answers its typed validation cause,
/// changes nothing and submits nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_native_tool_membership_refusal_preserves_its_cause() {
    use lash_core::facade_support::ToolStateFacadeOps as _;
    let core = standard_core_over(sqlite_memory_store_backend().await);
    core.session(crate::SessionId::parse("typed-tools").expect("id"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await
        .expect("created")
        .send(crate::TurnInput::text("materialize the session"))
        .output()
        .await
        .expect("the first turn answers");
    let session = core
        .session(crate::SessionId::parse("typed-tools").expect("id"))
        .open()
        .await
        .expect("open");
    let tools = session.admin().tools();
    let before = tools.state().await.expect("tool state");
    let error = tools
        .set_membership_many(&[("tool:absent".into(), false)])
        .await
        .expect_err("unknown membership refuses");
    assert!(
        matches!(error.source().and_then(|e| e.downcast_ref::<ReconfigureError>()), Some(ReconfigureError::Validation(message)) if message == "unknown tool id `tool:absent`"),
        "validation is typed: {error:?}"
    );
    let after = tools.state().await.expect("tool state");
    assert_eq!(
        before.recorded().map(|state| state.generation()),
        after.recorded().map(|state| state.generation())
    );
    assert_eq!(
        before.recorded().map(|state| state.tool_manifests()),
        after.recorded().map(|state| state.tool_manifests())
    );
    assert!(
        after.pending().is_empty(),
        "a refused change is never submitted"
    );
    assert!(error.is_terminal());
    assert!(!error.is_retryable());
    core.shutdown().await.expect("shutdown");
}

/// A plugin whose readiness decodes a stored value it cannot read (mode 1)
/// or encodes a command value that has no JSON form (mode 2). Mode 0
/// refuses nothing.
#[derive(Clone)]
struct StateHook {
    mode: Arc<AtomicUsize>,
}

impl PluginFactory for StateHook {
    fn id(&self) -> &'static str {
        "typed-state"
    }

    fn initialize_state(
        &self,
        _: &lash_core::RuntimeOwner,
        _: &lash_core::PluginConfig,
    ) -> std::result::Result<std::collections::BTreeMap<String, serde_json::Value>, PluginError>
    {
        Ok([("k".into(), serde_json::json!("text"))].into())
    }

    fn build(
        &self,
        _: &PluginSessionContext,
    ) -> std::result::Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

impl lash_core::plugin::PluginDefinition for StateHook {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("typed-state")
    }
}

impl SessionPlugin for StateHook {
    fn id(&self) -> &'static str {
        "typed-state"
    }

    fn register(&self, _registrar: &mut PluginRegistrar) -> std::result::Result<(), PluginError> {
        Ok(())
    }

    fn session_ready(&self, ctx: SessionReadyContext) -> std::result::Result<(), PluginError> {
        match self.mode.load(Ordering::SeqCst) {
            1 => {
                ctx.state.get_as::<u64>("k")?;
            }
            2 => {
                let keyed = std::collections::BTreeMap::from([((1, 2), 3)]);
                lash_core::plugin::StateCommands::new().set_as("k", &keyed)?;
            }
            _ => return Ok(()),
        }
        Err(PluginError::Registration(format!(
            "the state codec did not refuse; the namespace held {:?}",
            ctx.state.keys()
        )))
    }
}

/// The typed plugin-state error somewhere in `error`'s source chain.
fn state_source<'a>(error: &'a (dyn Error + 'static)) -> Option<&'a PluginStateError> {
    let mut current = Some(error);
    while let Some(error) = current {
        if let Some(state) = error.downcast_ref::<PluginStateError>() {
            return Some(state);
        }
        current = error.source();
    }
    None
}

/// A plugin's state-codec refusal at a cold session's readiness reaches the
/// host as a typed, terminal plugin-state error naming its key, both for a
/// value it cannot decode and for one it cannot encode.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "FIG-5324: a cold session's readiness sees an empty plugin namespace and a readiness refusal loses its typed cause"]
async fn a_native_cold_open_preserves_state_codec_refusals() {
    let backend = sqlite_memory_store_backend().await;
    for mode in 1..=2 {
        let id = format!("native-state-codec-{mode}");
        let mode_control = Arc::new(AtomicUsize::new(0));
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(StateHook {
                mode: Arc::clone(&mode_control),
            }))
            .build(crate::testing::runtime_lease_owner())
            .expect("core");
        let session = core
            .session(crate::SessionId::parse(&id).expect("id"))
            .create(crate::SessionCreation::root(mock_session_spec()))
            .await
            .expect("created");
        session
            .send(crate::TurnInput::text("ready"))
            .output()
            .await
            .expect("the session's first turn answers");
        core.shutdown().await.expect("shutdown");

        mode_control.store(mode, Ordering::SeqCst);
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .plugin(Arc::new(StateHook {
                mode: Arc::clone(&mode_control),
            }))
            .build(crate::testing::runtime_lease_owner())
            .expect("core");
        let error = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            core.session(crate::SessionId::parse(&id).expect("id"))
                .durable()
                .await
                .expect("handle")
                .send(crate::TurnInput::text("cold"))
                .output(),
        )
        .await
        .expect("the cold turn settles")
        .expect_err("readiness refuses");
        let state = state_source(&error)
            .unwrap_or_else(|| panic!("the facade keeps the typed state source: {error:?}"));
        match (mode, state) {
            (1, PluginStateError::Decode { key, .. })
            | (2, PluginStateError::Encode { key, .. }) => assert_eq!(key, "k"),
            other => panic!("wrong state fields: {other:?}"),
        }
        assert!(!error.is_retryable(), "{error:?}");
        assert!(error.is_terminal(), "{error:?}");
        core.shutdown().await.expect("shutdown");
    }
}
