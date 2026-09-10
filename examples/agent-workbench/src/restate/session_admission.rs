use super::*;
use lash::SessionId;

/// Journaled outcome of a workflow-entry session admission.
///
/// The fence read happens exactly once, on first execution; recording its
/// outcome in the journal keeps every later attempt on the same command
/// sequence. An unjournaled early-return refusal diverges replay (the session
/// can be retired between attempts) and trips a Restate journal mismatch,
/// which retries the invocation forever.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome")]
enum JournaledSessionAdmission {
    Admitted,
    Refused { message: String },
}

/// Admit `session_id` for `surface` through the session fence, journaling the
/// outcome so replays are deterministic. A refusal is the typed conflict.
pub(super) async fn journaled_session_admission(
    state: &AppState,
    controller: &lash_restate::RestateRuntimeEffectController<'_, WorkflowContext<'_>>,
    session_id: &SessionId,
    surface: &'static str,
) -> Result<(), AppError> {
    let admission_state = state.clone();
    let admission_session_id = SessionId::from(session_id.to_string());
    let Json(admission) = controller
        .context()
        .run(move || async move {
            let outcome = match admission_state
                .admit_session_id(&admission_session_id, surface)
                .await
            {
                Ok(()) => JournaledSessionAdmission::Admitted,
                Err(refusal) if refusal.verdict == AppErrorVerdict::Terminal => {
                    JournaledSessionAdmission::Refused {
                        message: refusal.message,
                    }
                }
                // Admission read failures (tombstone probe unavailable) stay
                // retryable inside the journaled step.
                Err(error) => return Err(HandlerError::from(error)),
            };
            Ok(Json(outcome))
        })
        .name(surface)
        .await
        .map_err(AppError::internal)?;
    match admission {
        JournaledSessionAdmission::Admitted => Ok(()),
        JournaledSessionAdmission::Refused { message } => Err(AppError::conflict(message)),
    }
}
