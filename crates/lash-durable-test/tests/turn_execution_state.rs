//! A turn's code-execution state and tool surface across its commit and its
//! frame switches, through a host's `send()` with the core's node serving
//! each turn in a runtime opened at the session's committed head (ADR 0132
//! §4, ADR 0113 §3.1).
//!
//! The core's protocol carries a scripted code executor: its state is a
//! byte string the turn's commit captures when it is dirty, and every
//! runtime the node opens restores it from the head, or a fresh frame's
//! state when the head holds none.
//!
//! - **Capture failure:** a capture that fails at the turn's commit commits
//!   nothing; the pass retries from the turn's last commit, re-sending its
//!   model call, and the turn commits once, with the state captured then.
//! - **Frame switch:** a committed frame switch clears the execution state:
//!   the follow-on's runtime restores the fresh frame's state, never the
//!   state the switch abandoned.
//! - **Rotation surface:** the follow-on of a frame switch sees the tools its
//!   provider advertises since the switch, under the session's hidden names
//!   and the host's curated membership.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::facade_support::ProviderHandle;
use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_core::{ToolControl, ToolOutcome};
use served::{Tier, WATCHDOG, World};

/// The state a fresh frame's executor starts from.
const FRESH: &[u8] = b"fresh-frame-execution-state";

/// The scripted executor: `snapshot` is its state, captured while `dirty`;
/// the next `fail_captures` captures fail.
#[derive(Default)]
struct Executor {
    dirty: AtomicBool,
    fail_captures: AtomicUsize,
    snapshot: Mutex<Vec<u8>>,
    restored: Mutex<Vec<Vec<u8>>>,
}

#[async_trait::async_trait]
impl lash_core::plugin::CodeExecutorPlugin for Executor {
    async fn execute_code(
        &self,
        _ctx: lash_core::RuntimeExecutionContext<'_>,
        _request: lash_core::ExecRequest,
    ) -> Result<lash_core::ExecResponse, lash_core::SessionError> {
        unreachable!("these laws run no code")
    }

    async fn frame_switch_carries(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        _successor: &lash_core::FrameNodeId,
        _initial_nodes: &[lash_core::SessionAppendNode],
    ) -> Result<Vec<lash_core::ArtifactName>, lash_core::SessionError> {
        Ok(Vec::new())
    }

    fn execution_state_dirty(&self) -> bool {
        self.dirty.load(Ordering::SeqCst)
    }

    async fn snapshot_execution_state(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
    ) -> Result<lash_core::plugin::ExecutionStateCapture, lash_core::SessionError> {
        if self
            .fail_captures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| {
                left.checked_sub(1)
            })
            .is_ok()
        {
            return Err(lash_core::SessionError::Protocol(
                "injected execution-state capture failure".to_owned(),
            ));
        }
        Ok(lash_core::plugin::ExecutionStateCapture::replace(
            self.snapshot.lock().unwrap().clone().into(),
        ))
    }

    async fn restore_execution_state(
        &self,
        _ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: &lash_core::plugin::HydratedExecutionState,
    ) -> Result<(), lash_core::SessionError> {
        self.restored.lock().unwrap().push(state.root.to_vec());
        Ok(())
    }
}

/// The protocol session: it restores the executor from the head's state,
/// or from [`FRESH`] when the head holds none.
struct Restores(Arc<Executor>);

#[async_trait::async_trait]
impl lash_core::plugin::ProtocolSessionPlugin for Restores {
    async fn restore_session(
        &self,
        ctx: lash_core::plugin::ProtocolSessionContext<'_>,
        state: lash_core::plugin::ProtocolSessionRestoreView,
    ) -> Result<(), lash_core::SessionError> {
        let state = state
            .execution_state
            .map_err(|source| lash_core::SessionError::Store {
                context: "hydrate the law's execution state".to_owned(),
                source,
            })?
            .unwrap_or_else(|| lash_core::plugin::HydratedExecutionState {
                root: FRESH.into(),
                components: std::collections::BTreeMap::new(),
            });
        lash_core::plugin::CodeExecutorPlugin::restore_execution_state(self.0.as_ref(), ctx, &state)
            .await
    }
}

/// A core over `backend` whose protocol carries `executor`, with `tools`.
fn core_with(
    backend: &lash::Backend,
    executor: &Arc<Executor>,
    tools: Arc<dyn lash_core::ToolProvider>,
) -> lash::LashCoreBuilder {
    let code_executor: Arc<dyn lash_core::plugin::CodeExecutorPlugin> = executor.clone();
    lash::LashCore::builder(backend.clone())
        .protocol_plugin(
            lash_core::testing::test_standard_protocol_factory_with_runtime_state(
                Arc::new(Restores(Arc::clone(executor))),
                Some(code_executor),
            ),
        )
        .tools(tools)
}

/// A model whose answer `respond` decides from each request and its
/// rendered transcript.
fn scripted(
    respond: impl Fn(&LlmRequest, &str) -> LlmResponse + Send + Sync + 'static,
) -> ProviderHandle {
    let respond = Arc::new(respond);
    lash_core::testing::TestProvider::builder()
        .kind("turn-execution-state")
        .requires_streaming(true)
        .complete(move |request: LlmRequest| {
            let respond = Arc::clone(&respond);
            async move {
                let transcript = format!("{:?}", request.messages);
                Ok(respond(&request, &transcript))
            }
        })
        .build()
        .into_handle()
}

fn call(id: &str, tool: &str) -> LlmResponse {
    served::response(vec![served::call(id, tool, serde_json::json!({}))])
}

/// Whether `request` already carries a tool result.
fn answered_a_call(request: &LlmRequest) -> bool {
    request.messages.iter().any(|message| {
        message.blocks.iter().any(|block| {
            matches!(
                block,
                lash_core::llm::types::LlmContentBlock::ToolResult { .. }
            )
        })
    })
}

/// The execution state `session`'s committed head holds.
async fn head_state(world: &World, session: &lash::DurableSession) -> Option<Vec<u8>> {
    let store = lash_core::store::SessionStore::new(
        world.backend.stores().session_store_factory(),
        session.session_id().clone(),
    )
    .unwrap();
    lash_core::store::load_session_window_state(&store, lash_core::store::WindowSelector::Current)
        .await
        .unwrap()
        .expect("the session has a head")
        .state
        .execution_state_snapshot()
        .as_deref()
        .map(<[u8]>::to_vec)
}

/// Wait until `session` has committed `count` turns.
async fn committed(session: &lash::DurableSession, count: usize) {
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let page = session
                .committed_turns(None, std::num::NonZeroU32::new(16).unwrap())
                .await
                .unwrap();
            if page.turns.len() >= count {
                assert_eq!(page.turns.len(), count, "{:#?}", page.turns);
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("the session never committed {count} turns"));
}

/// No tools.
fn no_tools() -> Arc<dyn lash_core::ToolProvider> {
    Arc::new(lash::tools::StaticToolProvider::new(Vec::new(), NoTool))
}

struct NoTool;

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for NoTool {
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        unreachable!("the session has no tools")
    }
}

/// A capture that fails at the commit commits nothing: the pass that met it
/// retries from the turn's last commit, re-sending its model call, and the
/// turn commits once, with the state the retry captured.
async fn a_failed_capture_commits_nothing_and_the_retried_pass_commits_the_turn(tier: Tier) {
    const BASELINE: &str = "commit the baseline";
    const FAILING: &str = "capture fails once";
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let model = {
        let asked = Arc::clone(&asked);
        scripted(move |request, transcript| {
            let input = if transcript.contains(FAILING) {
                FAILING
            } else {
                BASELINE
            };
            asked.lock().unwrap().push(input.to_owned());
            served::text(request, "answered")
        })
    };
    let executor = Arc::new(Executor::default());
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        core_with(backend, &executor, no_tools())
    })
    .await
    else {
        return;
    };
    let session = world.session("capture-failure", served::spec(8)).await;
    *executor.snapshot.lock().unwrap() = b"baseline".to_vec();
    executor.dirty.store(true, Ordering::SeqCst);
    served::assert_answered(BASELINE, &world.send(&session, BASELINE).await);
    assert_eq!(
        head_state(&world, &session).await.as_deref(),
        Some(&b"baseline"[..])
    );

    *executor.snapshot.lock().unwrap() = b"after-failure".to_vec();
    executor.fail_captures.store(1, Ordering::SeqCst);
    served::assert_answered(FAILING, &world.send(&session, FAILING).await);
    assert_eq!(
        executor.fail_captures.load(Ordering::SeqCst),
        0,
        "the capture failed once"
    );
    committed(&session, 2).await;
    assert_eq!(
        head_state(&world, &session).await.as_deref(),
        Some(&b"after-failure"[..]),
        "the turn commits the state its retry captured"
    );
    assert_eq!(
        asked
            .lock()
            .unwrap()
            .iter()
            .filter(|input| *input == FAILING)
            .count(),
        2,
        "the failed pass committed nothing, so its call is re-sent"
    );
    world.shutdown().await;
}

/// `switch_frame`'s body: it switches the agent frame with `task`.
struct SwitchFrame {
    task: &'static str,
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for SwitchFrame {
    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        ToolOutcome::ok(serde_json::json!({ "ok": true }))
            .with_control(ToolControl::SwitchAgentFrame {
                frame_key: lash_core::FrameKey::from_caller_material("execution-state-frame")
                    .unwrap(),
                initial_nodes: Vec::new(),
                task: Some(self.task.to_owned()),
            })
            .into()
    }
}

/// A committed frame switch clears the execution state its frame leaves:
/// the follow-on's runtime restores the fresh frame's state, and the head
/// holds none.
async fn a_committed_frame_switch_clears_the_execution_state(tier: Tier) {
    const ASK: &str = "switch frames";
    const TASK: &str = "carry on in the new frame";
    let model = scripted(|request, transcript| {
        if transcript.contains(TASK) {
            served::text(request, "carried on")
        } else {
            call("execution-state-switch", "switch_frame")
        }
    });
    let executor = Arc::new(Executor::default());
    let definition = lash_core::ToolDefinition::raw(
        "switch_frame",
        "switch_frame",
        "Switches the agent frame and hands the new frame a task.",
        serde_json::json!({ "type": "object", "additionalProperties": false, "properties": {} }),
        serde_json::json!({ "type": "object" }),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(120));
    let tools = Arc::new(lash::tools::StaticToolProvider::new(
        vec![definition],
        SwitchFrame { task: TASK },
    ));
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        core_with(backend, &executor, tools)
    })
    .await
    else {
        return;
    };
    let session = world.session("frame-switch-state", served::spec(8)).await;
    *executor.snapshot.lock().unwrap() = b"abandoned-frame-execution-state".to_vec();
    executor.dirty.store(true, Ordering::SeqCst);
    world.send(&session, ASK).await;
    // The follow-on's commit captures nothing of its own.
    executor.dirty.store(false, Ordering::SeqCst);
    committed(&session, 2).await;
    assert_eq!(
        head_state(&world, &session).await,
        None,
        "the switch cleared the abandoned frame's state"
    );
    let restored = executor.restored.lock().unwrap().clone();
    assert!(
        !restored
            .iter()
            .any(|state| state == b"abandoned-frame-execution-state"),
        "no runtime restored the abandoned state: {restored:?}"
    );
    assert_eq!(restored.last().map(Vec::as_slice), Some(FRESH));
    world.shutdown().await;
}

/// A provider whose manifests grow once `rotate_surface` switched the frame.
struct RotatingTools {
    rotated: AtomicBool,
}

fn rotating_definition(name: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "Exercises tool discovery across an agent frame rotation.",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(120))
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for RotatingTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        let mut names = vec!["rotate_surface", "curated_before_rotation"];
        if self.rotated.load(Ordering::SeqCst) {
            names.extend(["new_after_rotation", "hidden_after_rotation"]);
        }
        names
            .into_iter()
            .map(|name| rotating_definition(name).manifest())
            .collect()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.tool_manifests()
            .into_iter()
            .any(|manifest| manifest.name == name)
            .then(|| Arc::new(rotating_definition(name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name() {
            "rotate_surface" => {
                self.rotated.store(true, Ordering::SeqCst);
                ToolOutcome::ok(serde_json::json!({ "rotated": true })).with_control(
                    ToolControl::SwitchAgentFrame {
                        frame_key: lash_core::FrameKey::from_caller_material("rotated-surface")
                            .unwrap(),
                        initial_nodes: Vec::new(),
                        task: Some(ROTATED_TASK.to_owned()),
                    },
                )
            }
            "new_after_rotation" => ToolOutcome::ok(serde_json::json!({ "called": true }))
                .with_control(ToolControl::Finish {
                    value: lash_core::ToolValue::untrusted_json(serde_json::json!(
                        "new tool executed"
                    )),
                }),
            name => ToolOutcome::err_fmt(format_args!("`{name}` is not offered")),
        }
        .into()
    }
}

const ROTATED_TASK: &str = "call the newly available tool";

/// The follow-on of a frame switch is offered the tools its provider
/// advertises since the switch, never the session's hidden names nor a tool
/// the host curated out, and calls the new one.
async fn a_rotations_follow_on_is_offered_the_newly_advertised_tools(tier: Tier) {
    let offered = Arc::new(Mutex::new(Vec::<String>::new()));
    let model = {
        let offered = Arc::clone(&offered);
        scripted(move |request, transcript| {
            if transcript.contains(ROTATED_TASK) {
                if answered_a_call(request) {
                    return served::text(request, "done");
                }
                *offered.lock().unwrap() =
                    request.tools.iter().map(|tool| tool.name.clone()).collect();
                return call("rotation-new-tool", "new_after_rotation");
            }
            call("rotation-rotate", "rotate_surface")
        })
    };
    let executor = Arc::new(Executor::default());
    let tools = Arc::new(RotatingTools {
        rotated: AtomicBool::new(false),
    });
    let Some(world) = World::with_model(tier, Vec::new(), model, |backend| {
        core_with(backend, &executor, tools)
    })
    .await
    else {
        return;
    };
    let session = world.session("rotation-surface", served::spec(8)).await;
    let live = world
        .core
        .session(session.session_id().clone())
        .open()
        .await
        .unwrap();
    let config = live.admin().config();
    config
        .apply(
            lash::config::ConfigWrite::new("hide-after-rotation", config.revision().await.unwrap()),
            lash::config::ConfigTransaction::of(lash::config::SetToolAccess {
                access: lash_core::SessionToolAccess::ambient()
                    .with_hidden_tools(["hidden_after_rotation"])
                    .unwrap(),
            }),
        )
        .await
        .unwrap()
        .await_outcome(&config)
        .await
        .unwrap();
    live.admin()
        .tools()
        .set_membership(
            lash_core::ToolId::from("tool:curated_before_rotation"),
            false,
            "host:turn_execution_state:set_membership:483",
        )
        .await
        .unwrap()
        .settle_with(
            &live.admin().commands(),
            lash::testing::admin_fixture_outcome,
        )
        .await
        .unwrap();
    drop(live);
    world.send(&session, "rotate the frame").await;
    committed(&session, 2).await;
    let offered = offered.lock().unwrap().clone();
    assert!(
        offered.iter().any(|name| name == "new_after_rotation"),
        "the follow-on is offered the new tool: {offered:?}"
    );
    for absent in ["hidden_after_rotation", "curated_before_rotation"] {
        assert!(
            !offered.iter().any(|name| name == absent),
            "the follow-on is not offered `{absent}`: {offered:?}"
        );
    }
    world.shutdown().await;
}

tiered_laws!(
    a_failed_capture_commits_nothing_and_the_retried_pass_commits_the_turn,
    a_committed_frame_switch_clears_the_execution_state,
    a_rotations_follow_on_is_offered_the_newly_advertised_tools,
);
