use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
struct PredecessorTriggerIntentFixture {
    captured_from_endpoint_interruption: bool,
    invocation_body_bytes: Vec<u8>,
}

const TRIGGER_INTENT_CUTOVER_KEY: &str = "trigger-intent-cutover-v1";
const TRIGGER_INTENT_CUTOVER_SESSION: &str = "trigger-intent-cutover-session";
const TRIGGER_INTENT_CUTOVER_TURN: &str = "trigger-intent-cutover-turn";

#[restate_sdk::workflow]
trait TriggerIntentCutoverReplay {
    async fn run(input: Json<()>) -> HandlerResult<Json<serde_json::Value>>;
}

struct TriggerIntentCutoverReplayImpl {
    registry: Arc<dyn ProcessRegistry>,
    router: lash_core::facade_support::TriggerRouter,
}

impl TriggerIntentCutoverReplay for TriggerIntentCutoverReplayImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(()): Json<()>,
    ) -> HandlerResult<Json<serde_json::Value>> {
        let controller = RestateRuntimeEffectController::new(ctx);
        let scope =
            ExecutionScope::turn(TRIGGER_INTENT_CUTOVER_SESSION, TRIGGER_INTENT_CUTOVER_TURN);
        let attempt = controller
            .execute_effect(
                RuntimeEffectEnvelope::new(
                    lash_core::RuntimeEffectInvocation::new(
                        lash_core::EffectAddress::new(
                            scope.clone(),
                            "trigger-intent-cutover-attempt",
                        )
                        .expect("valid trigger-intent cutover address"),
                        lash_core::RuntimeAttribution::for_turn(
                            TRIGGER_INTENT_CUTOVER_SESSION,
                            TRIGGER_INTENT_CUTOVER_TURN,
                            0,
                            0,
                        ),
                        "trigger-intent-cutover-attempt",
                    ),
                    RuntimeEffectCommand::ToolAttempt {
                        call: prepared_tool_call_with(
                            "trigger-intent-cutover-call",
                            "trigger_intent_cutover",
                        ),
                        execution_grant: None,
                        attempt: 1,
                        max_attempts: 1,
                    },
                ),
                RuntimeEffectLocalExecutor::testing(|_| async {
                    Ok(RuntimeEffectOutcome::ToolAttempt {
                        launch: Box::new(lash_core::ToolAttemptLaunch::Done {
                            record: Box::new(completed_tool_record(
                                "trigger-intent-cutover-call",
                                "trigger_intent_cutover",
                            )),
                            intents: lash_core::ToolIntents {
                                protocol_version: 2,
                                intents: vec![lash_core::ToolIntent::EmitTrigger(
                                    lash_core::EmitTriggerIntent {
                                        session_id: SessionId::from(TRIGGER_INTENT_CUTOVER_SESSION),
                                        request: lash_core::TriggerOccurrenceRequest::new(
                                            "intent.cutover.trigger",
                                            "trigger-intent-cutover-source",
                                            serde_json::json!({"captured": true}),
                                            "predecessor-caller-key",
                                        ),
                                    },
                                )],
                            },
                        }),
                        triggers: Vec::new(),
                    })
                }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        let RuntimeEffectOutcome::ToolAttempt { launch, .. } = attempt else {
            return Err(TerminalError::new("cutover attempt returned the wrong effect").into());
        };
        let lash_core::ToolAttemptLaunch::Done { intents, .. } = *launch else {
            return Err(TerminalError::new("cutover attempt did not finish").into());
        };
        let outcomes = lash_core::testing::execute_tool_intents_with_services_and_trigger_router(
            controller
                .scoped_effect_controller(scope)
                .map_err(TerminalError::from_error)?,
            lash_core::testing::effect_backed_process_service(Arc::clone(&self.registry)),
            self.router.clone(),
            &SessionId::from(TRIGGER_INTENT_CUTOVER_SESSION),
            "trigger-intent-cutover-call",
            &intents,
        )
        .await
        .map_err(TerminalError::from_error)?;
        Ok(Json(
            serde_json::to_value(outcomes).map_err(TerminalError::from_error)?,
        ))
    }
}

async fn trigger_intent_cutover_endpoint() -> (
    Endpoint,
    Arc<lash_core::facade_support::InMemoryTriggerStore>,
) {
    let registry: Arc<dyn ProcessRegistry> =
        Arc::new(lash_core::TestLocalProcessRegistry::default());
    let store = Arc::new(lash_core::facade_support::InMemoryTriggerStore::default());
    let draft = lash_core::TriggerSubscriptionDraft::for_process(
        "test/trigger-intent-cutover",
        lash_core::ProcessExecutionEnvRef::new("process-env:trigger-intent-cutover"),
        "intent.cutover.trigger",
        "trigger-intent-cutover-source",
        lash_core::ProcessInput::Engine {
            kind: "test-engine".to_string(),
            payload: serde_json::json!({"process": "trigger-intent-cutover"}),
        },
        lash_core::ProcessIdentity::new("test-engine").with_label(Some("trigger-intent-cutover")),
    );
    store
        .execute_command(
            "trigger-intent-cutover-subscription",
            lash_core::TriggerCommand::Register {
                owner_scope: lash_core::TriggerOwnerScope::host("trigger-intent-cutover")
                    .expect("owner scope"),
                actor: lash_core::ProcessOriginator::host_scoped("trigger-intent-cutover"),
                draft,
            },
        )
        .await
        .expect("execute trigger subscription command")
        .expect("register trigger subscription");
    let router = lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&store) as Arc<dyn TriggerStore>,
        lash_core::testing::process_work_wiring_for_registry(Arc::clone(&registry)),
    );
    let endpoint = Endpoint::builder()
        .bind(TriggerIntentCutoverReplayImpl { registry, router }.serve())
        .build();
    (endpoint, store)
}

#[tokio::test]
async fn restate_ordinal_replay_refuses_v1_trigger_before_store_ingress() {
    let fixture: PredecessorTriggerIntentFixture = serde_json::from_slice(include_bytes!(
        "../../tests/fixtures/tool_intent_journals/v1-trigger-mid-drain.json"
    ))
    .expect("decode predecessor trigger-intent fixture");
    assert!(fixture.captured_from_endpoint_interruption);
    let (endpoint, store) = trigger_intent_cutover_endpoint().await;
    let response = invoke_endpoint_body(
        &endpoint,
        "TriggerIntentCutoverReplay",
        "run",
        bytes::Bytes::from(fixture.invocation_body_bytes),
    )
    .await
    .expect("replay predecessor trigger intent through Restate endpoint");
    let outcomes = restate_output_json::<Vec<lash_core::ToolIntentExecutionOutcome>>(&response)
        .expect("typed predecessor refusal output");
    assert!(matches!(
        outcomes.as_slice(),
        [lash_core::ToolIntentExecutionOutcome::Refused {
            kind: lash_core::ToolIntentKind::EmitTrigger,
            refusal: lash_core::ToolIntentRefusalReason::UnsupportedProtocolVersion { recorded: 1 },
            ..
        }]
    ));
    assert!(
        store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await
            .expect("read occurrences after predecessor replay")
            .is_empty()
    );
    assert!(
        store
            .list_deliveries()
            .await
            .expect("read deliveries after predecessor replay")
            .is_empty()
    );
}

#[tokio::test]
#[ignore = "explicit predecessor fixture capture utility"]
async fn capture_predecessor_trigger_intent_from_endpoint_interruption() {
    let (endpoint, _) = trigger_intent_cutover_endpoint().await;
    let interrupted = invoke_endpoint(
        &endpoint,
        "TriggerIntentCutoverReplay",
        "run",
        TRIGGER_INTENT_CUTOVER_KEY,
        &(),
    )
    .await
    .expect("interrupt at predecessor trigger ToolAttempt");
    let invocation_body =
        encode_captured_run_command_replay(TRIGGER_INTENT_CUTOVER_KEY, &(), &interrupted, &[], &[])
            .expect("capture completed predecessor trigger attempt");
    let fixture = PredecessorTriggerIntentFixture {
        captured_from_endpoint_interruption: true,
        invocation_body_bytes: invocation_body.to_vec(),
    };
    let mut bytes = serde_json::to_vec_pretty(&fixture).expect("serialize predecessor fixture");
    bytes.push(b'\n');
    std::fs::write(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/tool_intent_journals/v1-trigger-mid-drain.json"),
        bytes,
    )
    .expect("write predecessor trigger-intent fixture");
}
