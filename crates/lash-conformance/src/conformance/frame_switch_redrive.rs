//! FIG-3788: a driver turn that switches its agent frame, redriven after the
//! switch's commit, replays at the head it was admitted on.
//!
//! A drive admits one root and runs its physical turns in order: the first
//! turn's tool asks for a second agent frame, so that turn commits closed
//! with `AgentFrameSwitch` and the root continues in a follow-on turn. A
//! worker can die after that switch commit and before the root ended. The
//! tier then redrives the root, and the redrive replays its journal. By then
//! the store has moved on: the admitted input is consumed and the follow-on
//! is recorded on the head. A redrive that decided from that live state
//! would take another path than the journal holds (on Restate, a journal
//! mismatch at the switch's next call). The redrive must instead replay the
//! recorded admission, rebuild the root from the base it recorded, read the
//! switched turn's model and tool results back, and run only the follow-on.
//!
//! The law crashes the drive after the switched turn's commit and redrives
//! it on the tier's runner. The root must end with the follow-on frame's
//! answer, ask the model once per frame in total, run its tool once, and
//! commit each physical turn once.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

use crate::admit;
use crate::plugin::PluginFactory;

/// The tool whose call closes the first frame with a switch.
const SWITCH_TOOL: &str = "frame_switch_probe";

/// Panics as the first committed turn's delivery begins: the switch commit is
/// durable and the root has not ended.
struct PanicAfterSwitchCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAfterSwitchCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
            panic!("injected crash after the switched turn's commit");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn begin_named(&self, _phase: &str) {}
}

struct SwitchTool {
    executed: Arc<AtomicUsize>,
}

fn switch_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{SWITCH_TOOL}"),
        SWITCH_TOOL,
        "A tool whose call switches the turn to a follow-on agent frame.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for SwitchTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![switch_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == SWITCH_TOOL).then(|| Arc::new(switch_tool().contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: non-empty frame material always derives"
    )]
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"switched": true})).with_control(
                crate::ToolControl::SwitchAgentFrame {
                    frame_key: crate::FrameKey::from_caller_material("frame-switch-redrive")
                        .expect("non-empty frame material derives"),
                    initial_nodes: Vec::new(),
                    task: Some("frame-switch redrive follow-on".to_string()),
                },
            ),
        ))
    }
}

/// Everything a runtime for this law is built from, shared by every attempt
/// so each is the same session on the same store.
#[derive(Clone)]
struct RedriveParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimePersistence>,
    tool: Arc<dyn PluginFactory>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: RedriveParts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(
                crate::testing::test_standard_protocol_factories()
                    .into_iter()
                    .chain([parts.tool])
                    .collect(),
            )
            .with_store(parts.store)
            .build(),
    )
    .await
    .expect("build the frame-switch redrive conformance runtime")
}

type DriveResultTx = tokio::sync::mpsc::UnboundedSender<
    Result<crate::facade_support::QueuedTurnDrain<crate::AssembledTurn>, crate::RuntimeError>,
>;

/// One attempt at the root: the crashing one panics after the switch commit;
/// the redrive sends back how its drive ended.
fn attempt(
    parts: &RedriveParts,
    result_tx: Option<DriveResultTx>,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if result_tx.is_none() {
                runtime.set_turn_phase_probe(Arc::new(PanicAfterSwitchCommit));
            }
            let drive = Box::pin(runtime.stream_next_queued_work(crate::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                scope,
            )))
            .await;
            let Some(result_tx) = result_tx else {
                panic!(
                    "the crash probe did not fire after the switch commit: {:?}",
                    drive.map(crate::facade_support::QueuedTurnDrain::ran)
                );
            };
            let end = crate::ConformanceTurnEnd::of(&drive);
            let _ = result_tx.send(drive);
            end
        })
    })
}

/// A driver turn crashed after its frame switch committed and redriven
/// replays at its admitted head: the follow-on frame answers, the switched
/// turn's model call and tool run are read back, and every physical turn
/// commits once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_frame_switched_driver_turn_redriven_after_its_commit_replays_at_its_admitted_head(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let session_id = SessionId::from(format!("{prefix}-frame-switch-session"));
    let calls = Arc::new(AtomicUsize::new(0));
    let executed = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let calls = Arc::clone(&calls);
            move |_request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                async move {
                    let part = if index == 0 {
                        crate::LlmOutputPart::ToolCall {
                            call_id: "switch-call".into(),
                            tool_name: SWITCH_TOOL.into(),
                            input_json: "{}".into(),
                            replay: None,
                        }
                    } else {
                        crate::LlmOutputPart::Text {
                            text: "answered in the follow-on frame".into(),
                            response_meta: None,
                        }
                    };
                    Ok(crate::LlmResponse {
                        parts: vec![part],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    let mut host = crate::LawBackend::over_stores(Arc::clone(&stores), Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    store
        .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
            session_id.clone(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("switch frames, then answer"),
        ))
        .await
        .expect("accept the root's input");
    let before = store
        .load_session_head_meta()
        .await
        .expect("read the session head")
        .map_or(0, |head| head.head_revision);
    let tool: Arc<dyn PluginFactory> = Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-frame-switch-probe",
        crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool {
            executed: Arc::clone(&executed),
        })),
    ));
    let parts = RedriveParts {
        session_id: session_id.clone(),
        host,
        store: Arc::clone(&store),
        tool,
    };
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();
    let scope = admit(crate::ExecutionScope::queue_drain(
        &session_id,
        format!("{prefix}-frame-switch-drive"),
    ));
    // A redrive that diverged from its journal never ends: the tier retries
    // it until it rests. Bound the wait so the divergence fails the law.
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_crashed_then_redriven_turn(
            scope,
            attempt(&parts, None),
            attempt(&parts, Some(result_tx)),
        ),
    )
    .await
    .expect("the redrive after the switch commit ends (FIG-3788: it diverged from its journal)");
    let drive = result_rx
        .recv()
        .await
        .expect("the tier's runner ran the redriven drive")
        .unwrap_or_else(|error| panic!("the redrive after the switch commit replays: {error:?}"));
    let turn = drive.ran().expect("the redrive ran the root to its end");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the root finishes in the follow-on frame: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        turn.assistant_output.safe_text, "answered in the follow-on frame",
        "the follow-on frame answers"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "one model call per frame: the switched turn's call is read back, not asked again"
    );
    assert_eq!(
        executed.load(Ordering::SeqCst),
        1,
        "the switch tool ran once and its result is read back"
    );
    let committed = store
        .load_session_head_meta()
        .await
        .expect("read the committed head")
        .expect("the root committed");
    assert_eq!(
        committed.head_revision,
        before + 2,
        "the switched turn and its follow-on each commit once"
    );
    let pending = store
        .list_pending_turn_inputs(&session_id)
        .await
        .expect("read the pending inputs");
    assert!(
        pending.is_empty(),
        "the root's input is consumed: {pending:?}"
    );
}

/// Register the frame-switch redrive law (FIG-3788): a driver turn crashed
/// after its frame switch committed and redriven replays at its admitted
/// head. The fixture hands back a guard, a prefix, the tier's effect host,
/// the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! frame_switch_redrive_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn a_frame_switched_driver_turn_redriven_after_its_commit_replays_at_its_admitted_head() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            $crate::registration_macro_support::a_frame_switched_driver_turn_redriven_after_its_commit_replays_at_its_admitted_head(
                prefix, host, stores, runner,
            )
            .await;
            $crate::law_receipt::record(
                module_path!(),
                "a_frame_switched_driver_turn_redriven_after_its_commit_replays_at_its_admitted_head",
                "frame-switch-redrive",
            );
        }
    };
}
