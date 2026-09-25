//! FIG-3712: a child's authority comes from what its opener recorded at group
//! open, whether its opener lends the context or the deployment builds it.
//!
//! Each case runs the same child on both paths: a live opener's lent context,
//! and a deployment-built one. In both, the serving context is deliberately
//! wrong, so a path that let it decide shows up as its value surviving.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use super::tests::{IssueOrderRecorder, lent, manifest, request, spec};
use super::*;
use crate::ToolManifest;
use crate::runtime::effect::{ToolChildRebuildRefusal, UnrecordedSessionSources};

/// The recursive-spawn tool a subagent at its maximum depth has hidden.
const SPAWN: &str = "spawn_agent";

/// Answers every call, counting the spawns it was asked for.
struct SpawnAndEchoTools {
    spawned: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::ToolProvider for SpawnAndEchoTools {
    fn tool_manifests(&self) -> Vec<ToolManifest> {
        vec![manifest(SPAWN), manifest("echo-leaf")]
    }

    fn resolve_contract(&self, _name: &str) -> Option<Arc<crate::ToolContract>> {
        Some(Arc::new(crate::ToolContract::default()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if call.name() == SPAWN {
            self.spawned.fetch_add(1, Ordering::SeqCst);
        }
        crate::ToolOutcome::ok(serde_json::json!("done")).into()
    }
}

/// The two ways a child finds its context.
#[derive(Clone, Copy, Debug)]
enum Path {
    /// Its opener is live here and lends its own.
    Live,
    /// The deployment built one because its opener is not live here.
    Built,
}

fn serving_context(path: Path, dispatch: &ToolDispatchContext<'static>) -> LiveOpenerContext {
    match path {
        Path::Live => {
            let lent_controller = dispatch
                .effect_controller
                .scoped()
                .to_static()
                .expect("the lent dispatch's controller is 'static");
            LiveOpenerContext::capture(
                dispatch,
                lent_controller,
                tokio_util::sync::CancellationToken::new(),
            )
        }
        Path::Built => LiveOpenerContext::deployment_built(dispatch.clone()),
    }
}

fn max_depth_subagent() -> crate::SubagentSessionContext {
    crate::SubagentSessionContext {
        parent_session_id: crate::SessionId::from("parent-session"),
        capability: "explore".to_string(),
        depth: 3,
        max_depth: 3,
    }
}

/// Plugins built under `subagent`, as a subagent session's are.
fn plugins_under(
    subagent: Option<crate::SubagentSessionContext>,
) -> Arc<crate::plugin::PluginSession> {
    crate::plugin::PluginHost::empty()
        .build_session_with_parent(
            "opener-session",
            subagent
                .as_ref()
                .map(|subagent| subagent.parent_session_id.clone()),
            crate::plugin::SessionCreationConfig {
                authority: crate::plugin::SessionAuthorityContext {
                    subagent,
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .expect("plugin session")
}

/// A subagent at its maximum depth: `spawn_agent` is not in the surface its
/// opener recorded, because its tool access hides it.
fn max_depth_subagent_request() -> ToolChildRequest {
    let mut request = request();
    request.session.tool_surface = vec![crate::ToolDefinition {
        manifest: manifest("echo-leaf"),
        contract: crate::ToolContract::default(),
    }];
    request.session.tool_access = crate::SessionToolAccess::ambient()
        .with_hidden_tools([SPAWN])
        .expect("a valid hidden tool");
    request.session.subagent = Some(max_depth_subagent());
    request
}

/// A subagent at its maximum depth cannot spawn through a nested batch on
/// either path, even when the context serving it would admit `spawn_agent`:
/// the child's nested calls are admitted against the recorded surface, never
/// the serving context's catalog. Before FIG-3712 a built context, being a
/// fresh ambient session, admitted the spawn and reset the depth to one.
#[tokio::test]
async fn a_max_depth_subagent_cannot_spawn_through_a_nested_batch_on_either_path() {
    for path in [Path::Live, Path::Built] {
        let tools = Arc::new(SpawnAndEchoTools {
            spawned: AtomicUsize::new(0),
        });
        let mut serving = lent();
        let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
        std::mem::forget(event_rx);
        serving.event_tx = event_tx;
        serving.tools = Arc::clone(&tools) as Arc<dyn crate::ToolProvider>;
        // Both paths serve the subagent's own plugins: the opener's, or ones
        // built from the request.
        serving.plugins = plugins_under(Some(max_depth_subagent()));
        serving.tool_catalog = Arc::new(crate::ToolCatalog::from_tool_definitions(
            [SPAWN, "echo-leaf"]
                .into_iter()
                .map(|id| crate::ToolDefinition {
                    manifest: manifest(id),
                    contract: crate::ToolContract::default(),
                })
                .collect(),
        ));
        let context = serving_context(path, &serving);
        let request = max_depth_subagent_request();
        let controller = ScopedEffectController::shared(
            Arc::new(IssueOrderRecorder::new(false)),
            crate::AdmittedScope::turn("child-session", "turn"),
        )
        .expect("a valid child scope");
        let dispatch = Arc::new(
            rebind_child_dispatch(
                context.dispatch().as_ref(),
                &request,
                controller,
                spec(3),
                &ToolUsageLedger::new(),
            )
            .expect("the lent client's test service binds to any recorded authority"),
        );
        let wait = child_turn_cancel_wait(
            &dispatch,
            &request,
            &tokio_util::sync::CancellationToken::new(),
        );
        let body = child_tool_context(
            &dispatch,
            &request,
            wait,
            crate::tool_dispatch::OrchestratingChildSinks::default(),
        );

        let replies = crate::OrchestrationContext::new(body)
            .call_tool_batch(vec![
                crate::ToolInvocation::new(
                    "nested-spawn",
                    manifest(SPAWN).id,
                    serde_json::json!({}),
                ),
                crate::ToolInvocation::new(
                    "nested-echo",
                    manifest("echo-leaf").id,
                    serde_json::json!({}),
                ),
            ])
            .await;

        assert_eq!(replies.len(), 2, "{path:?}");
        assert!(
            !replies[0].output.is_success(),
            "{path:?}: the hidden spawn is refused: {:?}",
            replies[0]
        );
        assert!(
            replies[1].output.is_success(),
            "{path:?}: a recorded tool still runs: {:?}",
            replies[1]
        );
        assert_eq!(
            tools.spawned.load(Ordering::SeqCst),
            0,
            "{path:?}: no spawn ran"
        );
    }
}

/// A context whose plugins run under another subagent context than the
/// opener recorded is refused on either path, typed and before anything
/// runs: its plugins decide how deep a spawn recurses, and they cannot be
/// rebound. A subagent's child is never served by a root session's plugins,
/// nor a root session's child by a subagent's.
#[test]
fn a_context_under_another_subagent_context_is_refused_on_either_path() {
    for path in [Path::Live, Path::Built] {
        for (serving, recorded) in [
            (None, Some(max_depth_subagent())),
            (Some(max_depth_subagent()), None),
        ] {
            let mut lent = lent();
            lent.plugins = plugins_under(serving);
            let context = serving_context(path, &lent);
            let mut request = request();
            request.session.subagent = recorded;
            let error = rebind_child_dispatch(
                context.dispatch().as_ref(),
                &request,
                super::tests::child_controller(),
                spec(3),
                &ToolUsageLedger::new(),
            )
            .err()
            .unwrap_or_else(|| panic!("{path:?}: the disagreeing context is refused"));
            assert!(
                error
                    .message
                    .contains(&ToolChildRebuildRefusal::SubagentContext.to_string()),
                "{path:?}: {error}"
            );
            assert_eq!(
                error.turn_failure_cause(),
                crate::TurnFailureCause::LiveFault,
                "{path:?}: a refused child is retried, never settled"
            );
        }
    }
}

/// A built context has no link to its opener's cooperative cancellation, so
/// the turn's cancel reaches it only through the durable gate: the wait its
/// nested calls take observes the child's recorded authority on either path.
#[test]
fn a_built_childs_waits_observe_its_recorded_turn_gate() {
    for path in [Path::Live, Path::Built] {
        let context = serving_context(path, &lent());
        let request = request();
        let dispatch = Arc::new(
            rebind_child_dispatch(
                context.dispatch().as_ref(),
                &request,
                super::tests::child_controller(),
                spec(3),
                &ToolUsageLedger::new(),
            )
            .expect("the lent client's test service binds to any recorded authority"),
        );
        let wait =
            child_turn_cancel_wait(&dispatch, &request, &context.cancellation().child_token());
        let observed = wait
            .process_turn_cancellation()
            .unwrap_or_else(|| panic!("{path:?}: a recorded authority observes turn cancellation"));
        assert_eq!(
            observed.scope,
            crate::ExecutionScope::turn("child-session", "turn"),
            "{path:?}: the wait observes the child's recorded turn"
        );
    }
}

/// Each unrecorded source maps to its own refusal, and a request with none
/// is not refused.
#[test]
fn every_unrecorded_source_is_a_typed_rebuild_refusal() {
    assert_eq!(UnrecordedSessionSources::default().rebuild_refusal(), None);
    let cases = [
        (
            UnrecordedSessionSources {
                context_overlay_tools: true,
                ..Default::default()
            },
            ToolChildRebuildRefusal::ContextOverlayTools,
        ),
        (
            UnrecordedSessionSources {
                open_plugins: true,
                ..Default::default()
            },
            ToolChildRebuildRefusal::OpenPlugins,
        ),
        (
            UnrecordedSessionSources {
                fork_plugins: true,
                ..Default::default()
            },
            ToolChildRebuildRefusal::ForkPlugins,
        ),
        (
            UnrecordedSessionSources {
                open_provider: true,
                ..Default::default()
            },
            ToolChildRebuildRefusal::OpenProvider,
        ),
        (
            UnrecordedSessionSources {
                open_tool_policy: true,
                ..Default::default()
            },
            ToolChildRebuildRefusal::OpenToolPolicy,
        ),
        (
            UnrecordedSessionSources {
                plugin_state: true,
                ..Default::default()
            },
            ToolChildRebuildRefusal::PluginState,
        ),
    ];
    let host_refusals = [
        ToolChildRebuildRefusal::SessionServices,
        ToolChildRebuildRefusal::SubagentContext,
        ToolChildRebuildRefusal::AmbiguousDeployment,
    ];
    for refusal in host_refusals {
        assert_eq!(
            refusal.into_error("call-1").turn_failure_cause(),
            crate::TurnFailureCause::LiveFault,
            "{refusal}: a refused child is retried, never settled"
        );
    }
    for (sources, refusal) in cases {
        assert_eq!(sources.rebuild_refusal(), Some(refusal));
        let error = refusal.into_error("call-1");
        assert_eq!(error.code, crate::RuntimeErrorCode::PluginSessionManager);
        assert_eq!(
            error.turn_failure_cause(),
            crate::TurnFailureCause::LiveFault,
            "a refused child is retried, never settled"
        );
    }
}

/// A session read on a built context never answers; it fires the latch that
/// abandons the child's drive.
#[tokio::test]
async fn a_session_read_on_a_built_context_fires_the_refusal_latch() {
    let refusal = deployment_context::SessionServicesRefusal::default();
    let mut dispatch = lent();
    refusal.attach(&mut dispatch);
    let sessions = Arc::clone(&dispatch.sessions);
    let abandoned = refusal
        .abandoning(async move {
            sessions
                .snapshot_session(&crate::SessionId::from("child-session"))
                .await
                .map(|_| ())
        })
        .await;
    assert!(refusal.fired(), "the read fired the latch");
    assert_eq!(
        abandoned.expect_err("the refused read never answers the tool"),
        ToolChildRebuildRefusal::SessionServices
    );
}
