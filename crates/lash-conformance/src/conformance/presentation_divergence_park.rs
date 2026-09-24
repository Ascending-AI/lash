//! FIG-3679: a replay divergence at a tool-result presentation parks the
//! turn; the conflict never reaches the model as the tool's result.
//!
//! A tool call's presentation is a journaled `PresentToolResult` effect whose
//! recorded envelope a redrive must reproduce (ADR 0100). When it does not —
//! here, a tool whose preparation refusal names the pass it ran on, so the
//! settled output the redrive presents differs from the recorded one — the
//! recorded presentation answers a different request. Like any recorded
//! effect's replay hash conflict (FIG-3587), that parks the turn with a typed
//! `EffectReplayDivergence` naming `present_tool_result`: the model is not
//! asked again, the conflict is never shown to it as the tool's result, and
//! every later redrive refuses the same way.

use crate::admit;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::store::SessionCommitStore as _;
use lash_sansio::{SessionId, TurnId};

const TOOL: &str = "pass_refusal";

/// Panics when the effect loop ends: the model calls and the refused call's
/// presentation are journaled, and the turn has not committed.
struct PanicBeforeTurnCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicBeforeTurnCommit {
    fn begin(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}

    fn end(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::EffectLoop {
            panic!("injected crash after the presentation and before the turn commit");
        }
    }

    fn begin_named(&self, _phase: &str) {}
}

fn definition() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "A tool whose preparation refuses, naming the pass it ran on.",
        serde_json::json!({ "type": "object" }),
        serde_json::json!({ "type": "object" }),
    )
}

/// Refuses every call in preparation with a message naming `pass`: the
/// settled output a pass presents is that pass's own.
struct PassRefusal {
    pass: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl crate::ToolProvider for PassRefusal {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == TOOL).then(|| Arc::new(definition().contract()))
    }

    async fn prepare_tool_call(
        &self,
        _call: crate::ToolPrepareCall<'_>,
    ) -> Result<crate::PreparedToolCall, crate::ToolOutcome> {
        Err(crate::ToolOutcome::err_fmt(format!(
            "refused on pass {}",
            self.pass.load(Ordering::SeqCst)
        )))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        panic!("a call refused in preparation never executes");
    }
}

#[derive(Clone)]
struct Parts {
    session_id: SessionId,
    turn_id: TurnId,
    host: crate::RuntimeHostConfig,
    store: Arc<crate::InMemorySessionStore>,
    pass: Arc<AtomicUsize>,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime(parts: &Parts) -> crate::LashRuntime {
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(parts.session_id.clone());
    let state = crate::RuntimeSessionState {
        session_id: parts.session_id.clone(),
        policy: policy.clone(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let tools: Arc<dyn crate::ToolProvider> = Arc::new(PassRefusal {
        pass: Arc::clone(&parts.pass),
    });
    let factories = crate::testing::test_standard_protocol_factories()
        .into_iter()
        .chain([Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-pass-refusal",
            crate::facade_support::PluginSpec::new().with_tool_provider(tools),
        )) as Arc<dyn crate::facade_support::PluginFactory>])
        .collect();
    Box::pin(
        crate::LashRuntime::builder(parts.host.clone(), crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(factories)
            .with_store(Arc::clone(&parts.store) as Arc<dyn crate::RuntimePersistence>)
            .with_queued_work(Arc::new(crate::NoQueuedWork::new()))
            .build(),
    )
    .await
    .expect("build the presentation-divergence conformance runtime")
}

/// One pass of the law's turn: the crashing pass panics before its commit,
/// a redrive sends back what it returned.
fn attempt(
    parts: &Parts,
    pass: usize,
    result_tx: Option<
        tokio::sync::mpsc::UnboundedSender<Result<crate::AssembledTurn, crate::RuntimeError>>,
    >,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let result_tx = result_tx.clone();
        Box::pin(async move {
            parts.pass.store(pass, Ordering::SeqCst);
            let mut runtime = build_runtime(&parts).await;
            if result_tx.is_none() {
                runtime.set_turn_phase_probe(Arc::new(PanicBeforeTurnCommit));
            }
            let mut input = crate::TurnInput::text("call the refusing tool");
            input.trace_turn_id = Some(parts.turn_id.clone());
            let turn = runtime
                .stream_turn(
                    input,
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let Some(result_tx) = result_tx else {
                panic!("the crash probe did not fire before the turn commit: {turn:?}");
            };
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = result_tx.send(turn);
            end
        })
    })
}

/// Law: a redrive whose tool-result presentation diverged from its record
/// parks with `EffectReplayDivergence { effect_kind: "present_tool_result" }`,
/// shows the model nothing, and a second redrive refuses again.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_diverged_tool_presentation_parks_the_turn(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    _registry: Arc<dyn crate::ProcessRegistry>,
    _process_work: Arc<dyn crate::ProcessWorkSubstrate>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let model = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let requests = Arc::clone(&requests);
            move |request| {
                let rendered = format!("{request:?}");
                let first = {
                    let mut requests = requests
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    requests.push(rendered);
                    requests.len() == 1
                };
                async move {
                    let part = if first {
                        crate::LlmOutputPart::ToolCall {
                            call_id: "refused-call".to_string(),
                            tool_name: TOOL.to_string(),
                            input_json: "{}".to_string(),
                            replay: None,
                        }
                    } else {
                        crate::LlmOutputPart::Text {
                            text: "done".to_string(),
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
    let mut host = crate::LawBackend::in_process()
        .with_effect_host(Arc::clone(&effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(model.into_handle()));
    let parts = Parts {
        session_id: SessionId::from(format!("{prefix}-presentation-divergence-session")),
        turn_id: TurnId::from(format!("{prefix}-presentation-divergence-turn")),
        host,
        store: Arc::new(crate::InMemorySessionStore::new()),
        pass: Arc::new(AtomicUsize::new(0)),
    };
    let requests_seen = || {
        requests
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    };
    let (result_tx, mut result_rx) = tokio::sync::mpsc::unbounded_channel();

    // The live pass presents `refused on pass 1` to the model, then crashes
    // before its commit; the redrive presents `refused on pass 2`.
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                &parts.turn_id,
            )),
            attempt(&parts, 1, None),
            attempt(&parts, 2, Some(result_tx.clone())),
        )
        .await;
    let live_requests = requests_seen();
    assert_eq!(
        live_requests.len(),
        2,
        "the live pass asked the model twice: the call, then its presented result"
    );
    assert!(
        live_requests[1].contains("refused on pass 1"),
        "the live pass showed the model its own presented result"
    );

    for redrive in ["first", "second"] {
        let error = result_rx
            .recv()
            .await
            .expect("the redrive ran")
            .expect_err("a redrive whose presentation diverged refuses");
        assert!(
            error.code.parks_turn(),
            "{redrive} redrive: a presentation replay divergence parks the turn: {error:?}"
        );
        let park = parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("read the park")
            .expect("the diverged turn is parked");
        assert_eq!(park.turn_id, parts.turn_id);
        let crate::store::ParkReason::EffectReplayDivergence { effect_kind, .. } = &park.reason
        else {
            panic!("{redrive} redrive: the park names an effect replay divergence: {park:?}");
        };
        assert_eq!(
            effect_kind, "present_tool_result",
            "{redrive} redrive: the park names the diverged presentation"
        );
        assert_eq!(
            requests_seen(),
            live_requests,
            "{redrive} redrive: the model is asked nothing, and no conflict reaches it"
        );
        if redrive == "first" {
            runner
                .run_turn(
                    admit(crate::ExecutionScope::turn(
                        &parts.session_id,
                        &parts.turn_id,
                    )),
                    Box::new({
                        let attempt = attempt(&parts, 3, Some(result_tx.clone()));
                        move |scope| attempt(scope)
                    }),
                )
                .await;
        }
    }
}
