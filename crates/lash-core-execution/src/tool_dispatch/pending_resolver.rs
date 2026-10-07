//! Launching the start a parked call declared to resolve it, and the
//! receipts its launch leaves.
//!
//! A [`ToolOutcome::Pending`](crate::ToolOutcome::Pending) that carries a
//! [`PendingResolver`](crate::PendingResolver) makes the *runtime*
//! responsible for delivering the outcome. A round member that parks waits
//! on the tool completion wait its round pinned and, for a resolver, on the
//! terminal of the process the resolver names: the runner pins that
//! process-terminal wait in the transaction that records the park (L5's
//! `process_terminal` wait), so a crash leaves the park and its wait
//! together or neither.
//!
//! # Declared starts (ADR 0116 §3)
//!
//! A [`DeclaredStart`](crate::DeclaredStart) is staged before its call
//! parks, under its own start key beneath the call's lineage, and registered
//! by the commit that records the park, with the park's process-terminal
//! wait: a crash leaves the park, its child and its wait together or none of
//! them. The registrar mints the child's id when it prepares the
//! registration. The staged start, or its typed refusal, is the call's
//! launch receipt: its intent outcome for index 0.
//!
//! A declaration decodes without its constructor, so its bytes may name
//! another session, another call's identity or a nonzero index. The start
//! must name the admitted session, and its identity must be exactly the one
//! the declaring attempt derives for index 0. A mismatch is the call's typed
//! launch refusal, with nothing registered.

/// A declared start's launch receipt.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct LaunchReceipt {
    /// The realized start, or its typed refusal, as the call's intent
    /// outcome for index 0. It is host-facing metadata: the model sees the
    /// child's value only.
    pub outcome: crate::ToolIntentExecutionOutcome,
    /// The child the start registered, when it realized.
    pub process_id: Option<crate::ProcessId>,
}

/// Stage the start a parked call declared, under the start's own admission
/// beneath `scope`, holding the child for the call under `hold_key`: its
/// launch receipt, and the rows that register it with the park (see the
/// module documentation).
///
/// # Errors
///
/// A launch fault no retry of the start could settle as its result.
pub(crate) async fn launch_parked_start(
    processes: &dyn crate::ProcessService,
    scope: crate::ProcessOpScope<'_>,
    start: &crate::DeclaredStart,
    hold_key: String,
    cancels: bool,
    child_trace_hook: Option<&crate::ToolChildExecutionTraceHook>,
) -> Result<
    (
        LaunchReceipt,
        Option<crate::runtime::actor::round::StoreLocalEffect>,
    ),
    crate::RuntimeEffectControllerError,
> {
    // The child is registered under the call's hold, owned by the scope the
    // call runs under, so the row outlives every rerun of the start.
    let hold = consumer_hold_owner(&scope).map(|owner| crate::ConsumerHold {
        key: hold_key,
        owner,
        cancels,
    });
    let (outcome, effect) = super::intent_executor::realize_declared_start(
        processes,
        start,
        scope.with_consumer_hold(hold),
        child_trace_hook,
    )
    .await?;
    let process_id = match &outcome {
        crate::ToolIntentExecutionOutcome::Executed {
            realized: crate::ToolIntentRealized::StartProcess(handle),
            ..
        } => Some(handle.process_id.clone()),
        _ => None,
    };
    Ok((
        LaunchReceipt {
            outcome,
            process_id,
        },
        effect,
    ))
}

/// The launch receipt of a declaration that does not belong to its call:
/// refused under the call's own identity for index 0, before anything was
/// journaled or registered.
pub(super) fn unbound_declaration(
    declaring: &crate::ToolIntentIdentity,
    refusal: crate::ToolIntentRefusalReason,
) -> LaunchReceipt {
    tracing::warn!(
        target: "lash::tool_intent",
        tool_call_id = %declaring.tool_call_id,
        refusal_reason = %refusal.code(),
        "a declared start that does not belong to its call was refused before launch"
    );
    LaunchReceipt {
        outcome: crate::ToolIntentExecutionOutcome::Refused {
            identity: Some(declaring.clone()),
            intent_index: declaring.intent_index,
            kind: crate::ToolIntentKind::StartProcess,
            refusal,
        },
        process_id: None,
    }
}

/// The scope that owns a call's consumer hold: the starter the call's
/// declared start is admitted under. A scope with no start context owns no
/// hold, and its calls launch unheld.
fn consumer_hold_owner(scope: &crate::ProcessOpScope<'_>) -> Option<crate::ScopeId> {
    scope
        .start_cx()
        .ok()
        .flatten()
        .map(|cx| cx.starter().id().clone())
}

/// The failure a refused launch settles its call with. A start the registry
/// or its scope refused keeps that refusal's own code; a declaration refused
/// because it does not belong to its call is the declaring tool's fault, and
/// fails under the refusal's typed code.
pub(super) fn launch_refusal(outcome: &crate::ToolIntentExecutionOutcome) -> crate::ToolFailure {
    let (class, code, message) = match outcome {
        crate::ToolIntentExecutionOutcome::Refused {
            refusal: crate::ToolIntentRefusalReason::CommandFailed { cause },
            ..
        } => (
            cause.failure_class(),
            cause.code().to_string(),
            cause.to_string(),
        ),
        crate::ToolIntentExecutionOutcome::Refused { refusal, .. } => (
            crate::ToolFailureClass::Internal,
            refusal.code().to_string(),
            format!(
                "the declared start did not launch: {}",
                outcome.model_addendum()
            ),
        ),
        other => (
            crate::ToolFailureClass::Unavailable,
            "declared_start_refused".to_string(),
            format!(
                "the declared start did not launch: {}",
                other.model_addendum()
            ),
        ),
    };
    crate::ToolFailure::runtime(class, code, message)
}

/// The intent outcomes a call's model-facing return reports.
///
/// A parked call declares no intents, so the one outcome it can carry is its
/// declared start's launch receipt: host-facing metadata that names the child
/// (ADR 0116 §3.8). The model sees the child's value only, so the receipt is
/// left out of the return's addenda.
pub fn model_visible_intent_outcomes(
    outcome: &super::ToolDispatchOutcome,
) -> &[crate::ToolIntentExecutionOutcome] {
    model_visible_outcomes(&outcome.intents, &outcome.intent_outcomes)
}

/// The outcomes of `outcomes` a model-facing return reports, for a call that
/// declared `intents`: a parked call's lone launch receipt is left out (see
/// [`model_visible_intent_outcomes`]).
pub(crate) fn model_visible_outcomes<'a>(
    intents: &crate::ToolIntents,
    outcomes: &'a [crate::ToolIntentExecutionOutcome],
) -> &'a [crate::ToolIntentExecutionOutcome] {
    let launch_receipt = intents.intents.is_empty()
        && matches!(
            outcomes,
            [crate::ToolIntentExecutionOutcome::Executed {
                realized: crate::ToolIntentRealized::StartProcess(_),
                ..
            } | crate::ToolIntentExecutionOutcome::Refused {
                kind: crate::ToolIntentKind::StartProcess,
                ..
            }]
        );
    if launch_receipt { &[] } else { outcomes }
}

#[cfg(test)]
mod intent_shape_laws {
    use super::*;

    #[test]
    fn terminal_start_conflict_keeps_its_failure_class_and_typed_key() {
        let key = crate::StartKey::for_host("conflicting-start");
        let error = crate::PluginError::StartKeyConflict {
            start_key: key.clone(),
        };
        let outcome = crate::ToolIntentExecutionOutcome::Refused {
            identity: None,
            intent_index: 0,
            kind: crate::ToolIntentKind::StartProcess,
            refusal: crate::ToolIntentRefusalReason::CommandFailed {
                cause: crate::ToolIntentCommandFailure::from(&error),
            },
        };
        let failure = launch_refusal(&outcome);
        assert_eq!(failure.class, crate::ToolFailureClass::InvalidRequest);
        let bytes = serde_json::to_value(&outcome).expect("record refusal");
        assert_eq!(
            bytes["refusal"]["cause"]["message"]["start_key"],
            serde_json::to_value(&key).expect("key")
        );
        let widened = crate::PluginError::RuntimeEffectController(error.into());
        let cause = crate::ToolIntentCommandFailure::from(&widened);
        assert_eq!(
            cause.failure_class(),
            crate::ToolFailureClass::InvalidRequest
        );
        assert_eq!(cause.code(), "process_start_key_conflict");
        let bytes = serde_json::to_value(cause).expect("record widened cause");
        assert_eq!(
            bytes["message"]["cause"]["start_key"],
            serde_json::to_value(key).expect("key")
        );
    }

    #[test]
    fn realized_start_cannot_decode_without_its_handle() {
        let identity = crate::derive_tool_intent_identity(
            &crate::RuntimeOwner::Session(crate::SessionId::from("session")),
            "turn",
            &crate::ToolCallId::fixture("call"),
            0,
        );
        for result in [serde_json::Value::Null, serde_json::json!({})] {
            let mut bytes = serde_json::json!({
                "status": "executed", "identity": identity,
                "kind": "start_process", "result": result,
                "realized": {"kind": "start_process", "result": result},
            });
            assert!(
                serde_json::from_value::<crate::ToolIntentExecutionOutcome>(bytes.clone()).is_err(),
                "a durable start must carry a complete handle"
            );
            let object = bytes.as_object_mut().expect("receipt object");
            object.remove("kind");
            object.remove("result");
            assert!(
                serde_json::from_value::<crate::ToolIntentExecutionOutcome>(bytes).is_err(),
                "the realized payload must carry the complete handle"
            );
        }
    }
}
