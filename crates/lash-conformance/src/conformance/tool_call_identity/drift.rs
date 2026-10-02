//! A call's retained request is bound to its identity: a redrive that would
//! run the call under the same identity with a different request is refused
//! before any effect (ADR 0117 §7, ADR 0116 §2.2).

use super::laws::crash_while_held_result;
use super::{DRIFTING, ProbeArgs, ToolCallIdentityTier, World, assert_finished, calls, text};
use crate::runtime::effect::ToolChildAdmission;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// A changed prepare result reuses the originally retained payload (ADR 0117).
pub async fn retained_payload_drift_is_refused_before_effects(tier: ToolCallIdentityTier) {
    let world = World::new(&tier, "payload-drift");
    let turn = world.turn(
        "turn",
        vec![
            calls(&[("call_drift", DRIFTING, ProbeArgs::label("drift"))]),
            text("the drifting call settled"),
        ],
    );
    let ended = crash_while_held_result(&world, &turn, "drift").await;
    let assembled = ended
        .as_ref()
        .unwrap_or_else(|error| panic!("prepared-payload replay must succeed: {error}"));
    assert_finished("prepared-payload replay", assembled);
    let prepares = world.witness.prepares.load(Ordering::SeqCst);
    let admitted = serde_json::json!({ "seal": 1 });
    let executions = world.witness.of("drift");
    assert!(
        !executions.is_empty(),
        "the retained prepared payload must execute"
    );
    for execution in &executions {
        assert_eq!(
            execution.prepared, admitted,
            "an effect ran with a payload other than the one sealed at admission (the prepare \
             phase ran {prepares} times; the redrive ended {ended:?}): {executions:?}"
        );
    }
    retained_request_replay(&tier, "prepared-payload").await;
    eprintln!(
        "retained_payload_drift_is_refused_before_effects: prepared {prepares} time(s), \
         {} effect run(s), the redrive {}",
        executions.len(),
        match &ended {
            Ok(turn) => format!("ended {:?}", turn.outcome),
            Err(error) => format!("refused: {error}"),
        }
    );
}

/// A replay changes the request bound to one call, after its binding became
/// durable and before any external effect. Refusal preserves that binding.
pub async fn retained_call_identity_refuses_name_arguments_and_authority_drift_before_effects(
    tier: ToolCallIdentityTier,
) {
    for change in [
        "name",
        "arguments",
        "source",
        "execution-binding",
        "admission",
    ] {
        retained_request_replay(&tier, change).await;
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance setup and bounded attempt reports"
)]
async fn retained_request_replay(tier: &ToolCallIdentityTier, change: &str) {
    let session_id = lash_sansio::SessionId::fixture(format!("{}-request-{change}", tier.prefix));
    let admitted = crate::AdmittedScope::turn(session_id.clone(), "test-turn");
    let definition = super::probe_definition(super::PROBE);
    let original = crate::PreparedToolCall {
        call_id: crate::ToolCallId::fixture("retained-call"),
        provider_call_id: Some("provider-call".to_string()),
        tool_id: definition.id().clone(),
        tool_name: super::PROBE.to_string(),
        args: serde_json::json!({"label": "original"}),
        replay: None,
        prepared_payload: serde_json::json!({"seal": 1}),
    };
    let grant = crate::ToolExecutionGrant::from_definition(
        crate::plugin::PluginRevision::new(
            crate::PLUGIN_TOOL_SOURCE_ID,
            crate::plugin::BehaviorRevision::ONE,
        ),
        definition.clone(),
    )
    .with_source_id("original-source")
    .with_execution_binding(serde_json::json!({"owner": "original"}));
    let authority = ToolChildAdmission::Granted {
        grant: Box::new(grant.clone()),
    };
    let mut changed = original.clone();
    changed.prepared_payload = serde_json::json!({"seal": 2});
    let mut changed_authority = authority.clone();
    match change {
        "name" => changed.tool_name = "different-name".to_string(),
        "arguments" => changed.args = serde_json::json!({"label": "different"}),
        "source" => {
            changed_authority = ToolChildAdmission::Granted {
                grant: Box::new(grant.clone().with_source_id("different-source")),
            }
        }
        "execution-binding" => {
            changed_authority = ToolChildAdmission::Granted {
                grant: Box::new(
                    grant.with_execution_binding(serde_json::json!({"owner": "different"})),
                ),
            }
        }
        "admission" => {
            changed_authority = ToolChildAdmission::Catalog {
                owner: crate::plugin::PluginRevision::new(
                    crate::PLUGIN_TOOL_SOURCE_ID,
                    crate::plugin::BehaviorRevision::ONE,
                ),
                manifest: Box::new(definition.manifest()),
            }
        }
        "prepared-payload" => {}
        _ => panic!("unknown request change: {change}"),
    }
    let effects = Arc::new(AtomicUsize::new(0));
    let (answers, mut results) = tokio::sync::mpsc::unbounded_channel();
    let crashing = request_attempt(
        &session_id,
        original.clone(),
        authority.clone(),
        &effects,
        answers.clone(),
        RequestAttemptEnd::CrashBeforeEffects,
    );
    let changed_attempt = request_attempt(
        &session_id,
        changed,
        changed_authority,
        &effects,
        answers.clone(),
        if change == "prepared-payload" {
            RequestAttemptEnd::Settle
        } else {
            RequestAttemptEnd::CrashAfterAnswer
        },
    );
    if change == "prepared-payload" {
        tier.runner
            .run_crashed_then_redriven_turn(admitted, crashing, changed_attempt)
            .await;
        let (result, at_answer) = tokio::time::timeout(super::PATIENCE, results.recv())
            .await
            .expect("redrive answers")
            .expect("redrive reports its result");
        assert_eq!(
            result.expect("a new prepared payload reuses the retained payload"),
            vec![Some(serde_json::json!({"seal": 1}))]
        );
        assert_eq!(at_answer, 1);
    } else {
        let mut restored = original;
        restored.prepared_payload = serde_json::json!({"seal": 99});
        let restoration = request_attempt(
            &session_id,
            restored,
            authority,
            &effects,
            answers,
            RequestAttemptEnd::Settle,
        );
        tier.runner
            .run_crashes_then_redriven_turn(admitted, vec![crashing, changed_attempt], restoration)
            .await;
        let (result, at_refusal) = tokio::time::timeout(super::PATIENCE, results.recv())
            .await
            .expect("refused replay answers")
            .expect("refused replay reports its result");
        let error = result.expect_err("identity drift must refuse before the external effect");
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::LashlangCellBindingDrift
        );
        assert_eq!(at_refusal, 0, "{change} crossed the effect fence");
        let (retained, at_restoration) = tokio::time::timeout(super::PATIENCE, results.recv())
            .await
            .expect("restoration answers")
            .expect("restoration reports its result");
        assert_eq!(
            retained.expect("the original binding still admits its request"),
            vec![Some(serde_json::json!({"seal": 1}))],
            "refusal changed the retained binding"
        );
        assert_eq!(
            at_restoration, 1,
            "the admitted request executes its effect"
        );
    }
    assert_eq!(effects.load(Ordering::SeqCst), 1);
    eprintln!(
        "retained request replay: {change}; original payload retained; exactly one admitted effect"
    );
    tier.runner.scenario_finished().await;
}

#[derive(Clone, Copy)]
enum RequestAttemptEnd {
    CrashBeforeEffects,
    CrashAfterAnswer,
    Settle,
}

type BindingResult = Result<Vec<Option<serde_json::Value>>, crate::RuntimeEffectControllerError>;

fn request_attempt(
    session_id: &lash_sansio::SessionId,
    call: crate::PreparedToolCall,
    authority: ToolChildAdmission,
    effects: &Arc<AtomicUsize>,
    answers: tokio::sync::mpsc::UnboundedSender<(BindingResult, usize)>,
    attempt_end: RequestAttemptEnd,
) -> crate::ConformanceTurnAttempt {
    let session_id = session_id.clone();
    let effects = Arc::clone(effects);
    Arc::new(move |scoped| {
        let session_id = session_id.clone();
        let call = call.clone();
        let authority = authority.clone();
        let effects = Arc::clone(&effects);
        let at_answer = Arc::clone(&effects);
        let answers = answers.clone();
        Box::pin(async move {
            let context = crate::testing::TestExecutionContextBuilder::over_controller(scoped)
                .session_id(session_id)
                .build()
                .into_runtime();
            let bound = crate::testing::runtime_internals::bind_retained_tool_requests(
                &context,
                "retained-group",
                &[(&call, &authority)],
            )
            .await;
            if matches!(attempt_end, RequestAttemptEnd::CrashBeforeEffects) {
                assert!(
                    bound.is_ok(),
                    "initial request binds before the crash: {bound:?}"
                );
                panic!("crash after retaining the call request, before its external effect");
            }
            let result = match bound {
                Ok(retained) => context
                    .journaled_language_value_with(
                        "external-effect".to_string(),
                        "probe-external-effect".to_string(),
                        move || async move {
                            effects.fetch_add(1, Ordering::SeqCst);
                            Ok(serde_json::Value::Null)
                        },
                    )
                    .await
                    .map(|_| retained),
                Err(error) => Err(error),
            };
            let end = match &result {
                Ok(_) => crate::ConformanceTurnEnd::Settled,
                Err(error) => crate::ConformanceTurnEnd::Aborted(error.turn_failure_cause()),
            };
            let _ = answers.send((result, at_answer.load(Ordering::SeqCst)));
            if matches!(attempt_end, RequestAttemptEnd::CrashAfterAnswer) {
                panic!("crash after checking drift refusal, before restoring the original request");
            }
            end
        })
    })
}
