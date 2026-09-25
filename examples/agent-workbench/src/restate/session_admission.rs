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

/// The error a turn handler ends its attempt with when `error` refused it,
/// once any generation refusal has parked the turn of `scope` (FIG-3735).
///
/// A refused redrive of a turn still in flight replays a journal that
/// already holds its later commands: the turn parks with the typed
/// generation refusal and the attempt ends [`AppErrorVerdict::Parked`], so
/// the invocation keeps its journal and pauses after its attempt budget. A
/// park the store cannot read or write parks the attempt all the same, and
/// its retry writes the park. Anything else, a refusal with no turn in
/// flight included, is `otherwise(error)`.
pub(super) async fn park_generation_refused_turn(
    state: &AppState,
    scope: lash::runtime::ExecutionScope,
    error: lash::EmbedError,
    otherwise: fn(lash::EmbedError) -> AppError,
) -> AppError {
    let Some(refusal) = error.session_state_version_refusal() else {
        return otherwise(error);
    };
    let parked = lash_restate::park_generation_refused_turn(
        state.session_store_factory.as_ref(),
        &scope,
        refusal,
        state.core.backend().clock().timestamp_ms(),
    )
    .await;
    match parked {
        Ok(None) => otherwise(error),
        Ok(Some(_)) | Err(_) => AppError {
            status: axum::http::StatusCode::CONFLICT,
            message: format!(
                "turn `{}` parked: its redrive was refused by the session-state generation \
                 gate (found {}, current {}): {error}",
                scope.id(),
                refusal.found,
                refusal.current
            ),
            verdict: AppErrorVerdict::Parked,
            retirement: None,
        },
    }
}

/// `result`, or the error its attempt ends with: a generation refusal parks
/// the turn of `scope` when one is in flight (see
/// [`park_generation_refused_turn`]), and anything else is `otherwise`.
pub(super) async fn or_park_refused<T>(
    state: &AppState,
    scope: &lash::runtime::ExecutionScope,
    result: Result<T, lash::EmbedError>,
    otherwise: fn(lash::EmbedError) -> AppError,
) -> Result<T, AppError> {
    match result {
        Ok(value) => Ok(value),
        Err(error) => {
            Err(park_generation_refused_turn(state, scope.clone(), error, otherwise).await)
        }
    }
}
