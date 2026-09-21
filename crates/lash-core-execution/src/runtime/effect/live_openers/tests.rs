use std::sync::Arc;

use super::{LiveOpenerContext, LiveOpenerRegistry};
use crate::{EffectOpener, ProcessId, ProcessRef, SessionId};

/// A dispatch context is 24 fields of deployment wiring; the registry cares
/// about none of them, so one throwaway is enough for every case here.
fn live_context() -> LiveOpenerContext {
    struct NoopTools;
    #[async_trait::async_trait]
    impl crate::ToolProvider for NoopTools {
        fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
            Vec::new()
        }

        fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
            None
        }

        async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            crate::ToolOutcome::err_fmt("the registry never runs a tool").into()
        }
    }

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(1);
    // Held for the registry's lifetime in each test, so the sender never
    // reports a closed channel for a reason unrelated to what is asserted.
    std::mem::forget(event_rx);
    let dispatch = crate::tool_dispatch::ToolDispatchContext {
        plugins: crate::plugin::PluginHost::empty()
            .build_session("session")
            .expect("plugin session"),
        tools: Arc::new(NoopTools),
        tool_registry: None,
        tool_catalog: Arc::new(crate::ToolCatalog::from_tool_definitions(Vec::new())),
        sessions: Arc::new(crate::testing::MockSessionManager::default()),
        session_lifecycle: Arc::new(crate::testing::MockSessionManager::default()),
        session_graph: Arc::new(crate::testing::MockSessionManager::default()),
        processes: Arc::new(crate::UnavailableProcessService),
        trigger_router: None,
        process_definitions: None,
        process_engines: crate::ProcessEngineRegistry::default(),
        effect_controller: crate::runtime::RuntimeEffectControllerHandle::shared(Arc::new(
            crate::NativeRuntimeEffectController::default(),
        )),
        direct_completions: crate::DirectCompletionClient::unavailable(
            "direct completions are unavailable in this test context",
        ),
        parent_invocation: None,
        execution_env_spec: crate::ProcessExecutionEnvSpec::new(
            crate::PluginOptions::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
        ),
        session_id: SessionId::from("session"),
        agent_frame_id: crate::FrameNodeId::new("test-frame").expect("frame id"),
        event_tx,
        checkpoint_messages: crate::tool_dispatch::CheckpointMessageBuffer::default(),
        trigger_outcomes: crate::tool_dispatch::ToolTriggerOutcomeBuffer::default(),
        attachment_store: Arc::new(crate::SessionAttachmentStore::in_memory()),
        attachment_source_policy: Arc::new(crate::OpenAttachmentSourcePolicy),
        turn_context: crate::TurnContext::default(),
        clock: Arc::new(crate::SystemClock),
    };
    LiveOpenerContext::capture(&dispatch).expect("a shared controller lends a 'static context")
}

fn process_opener(name: &str, incarnation: u64) -> EffectOpener {
    EffectOpener::process(ProcessRef::new(
        ProcessId::from(name),
        crate::ProcessIncarnation::from_registration_sequence(incarnation),
    ))
}

/// The rule the whole mechanism rests on: a child whose opener is not live
/// here is **not ours to run**, and that is reported as absence rather than as
/// a failure.
#[test]
fn an_opener_this_host_does_not_run_is_absent_rather_than_an_error() {
    let registry = Arc::new(LiveOpenerRegistry::new());
    let absent = EffectOpener::turn("session", "turn-1");

    assert!(registry.context_for(&absent).is_none());
    assert!(!registry.is_live(&absent));
    assert!(registry.is_empty());
}

/// Two openers that would render to colliding text are distinct keys, because
/// the key is the value (ADR 0099 §1).
#[test]
fn openers_are_keyed_by_value_not_by_rendered_text() {
    let registry = Arc::new(LiveOpenerRegistry::new());
    // A turn whose session id is spelled exactly like a process opener's
    // `{process_id}#{incarnation}` rendering.
    let turn_like_a_process = EffectOpener::turn("indexer#7", "turn-1");
    let real_process = process_opener("indexer", 7);

    let _guard = registry.register(turn_like_a_process.clone(), live_context());

    assert!(registry.is_live(&turn_like_a_process));
    assert!(
        !registry.is_live(&real_process),
        "a process opener must not be found through a turn whose text renders the same way; \
         that collision is the aliasing section 1 refuses"
    );
    assert_eq!(registry.len(), 1);
}

/// A process re-registered under the same name is a different opener, so it
/// does not inherit its predecessor's children.
#[test]
fn a_new_incarnation_is_a_different_opener() {
    let registry = Arc::new(LiveOpenerRegistry::new());
    let first = process_opener("indexer", 1);
    let second = process_opener("indexer", 2);

    let _guard = registry.register(first.clone(), live_context());

    assert!(registry.is_live(&first));
    assert!(
        !registry.is_live(&second),
        "the reusable name is not the opener: a second incarnation must not find the first's \
         registration"
    );
}

/// Dropping the owner's guard is the deregistration.
#[test]
fn dropping_the_guard_deregisters_the_opener() {
    let registry = Arc::new(LiveOpenerRegistry::new());
    let opener = EffectOpener::turn("session", "turn-1");

    let guard = registry.register(opener.clone(), live_context());
    assert!(registry.is_live(&opener));

    drop(guard);
    assert!(
        !registry.is_live(&opener),
        "an opener this worker is no longer running must stop lending its context"
    );
    assert!(registry.is_empty());
}

/// A redrive re-registers the same opener, and the superseded worker's guard
/// must not evict the newcomer when it finally drops.
///
/// This is the case that makes "recovered while the opener lives" work on these
/// tiers: the old and new workers overlap, and the old one's teardown is
/// concurrent with the new one's registration.
#[test]
fn a_superseded_registration_does_not_evict_its_replacement() {
    let registry = Arc::new(LiveOpenerRegistry::new());
    let opener = EffectOpener::turn("session", "turn-1");

    let stale = registry.register(opener.clone(), live_context());
    let fresh = registry.register(opener.clone(), live_context());
    assert_eq!(
        registry.len(),
        1,
        "one opener is one entry however often it redrives"
    );

    drop(stale);
    assert!(
        registry.is_live(&opener),
        "the redriven worker's registration stands: a predecessor winding down must not strand \
         the children the successor is there to run"
    );

    drop(fresh);
    assert!(!registry.is_live(&opener));
}

/// Two hosts in one process do not see each other's openers.
#[test]
fn registries_are_per_host_with_no_shared_state() {
    let first = Arc::new(LiveOpenerRegistry::new());
    let second = Arc::new(LiveOpenerRegistry::new());
    let opener = EffectOpener::turn("session", "turn-1");

    let _guard = first.register(opener.clone(), live_context());

    assert!(first.is_live(&opener));
    assert!(
        !second.is_live(&opener),
        "a child must never run against a deployment that did not admit it"
    );
}
