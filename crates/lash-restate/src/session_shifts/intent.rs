use lash_core::SessionId;
use lash_core::engine::{
    Admitted, BuildGeneration, RunOutcome, ShiftLoop, ShiftRequest, ShiftStop,
};
use serde::{Deserialize, Serialize};

/// The request `LashSession/{session}/shift` runs: one shift of the session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateSessionShiftRequest {
    pub request: ShiftRequest,
    /// What the leg that handed this shift off remembers of its own runs
    /// ([`ShiftLoop::handed_off`]): the stop rules this leg starts from, so a
    /// run that leg ran is not run again here. Only a leg's own continuation
    /// send carries it; a host's send and a waiter's attach never do.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handed_off: Option<ShiftLoop>,
}

/// One immutable shift intent dispatched before its run is selected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateRunRequest {
    pub sender_generation: Option<BuildGeneration>,
    pub request: ShiftRequest,
    pub ordinal: u32,
    pub rules: ShiftLoop,
    pub draining: Option<BuildGeneration>,
}

/// The recorded answer of a turn invocation, including admission-only stops.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "invocation_outcome", rename_all = "snake_case")]
pub enum RestateRunOutcome {
    Stopped {
        stop: ShiftStop,
    },
    Ran {
        admitted: Admitted,
        outcome: RunOutcome,
    },
}

/// The one durable value retained by a turn, before and after execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(super) enum LashTurnState {
    Selected {
        admitted: Admitted,
    },
    Stopped {
        stop: ShiftStop,
    },
    Ran {
        admitted: Admitted,
        outcome: RunOutcome,
    },
}

impl LashTurnState {
    pub(super) fn into_admission(self) -> Option<Admitted> {
        match self {
            Self::Selected { admitted } | Self::Ran { admitted, .. } => Some(admitted),
            Self::Stopped { .. } => None,
        }
    }

    pub(super) fn into_outcome(self) -> Option<RestateRunOutcome> {
        match self {
            Self::Selected { .. } => None,
            Self::Stopped { stop } => Some(RestateRunOutcome::Stopped { stop }),
            Self::Ran { admitted, outcome } => Some(RestateRunOutcome::Ran { admitted, outcome }),
        }
    }

    pub(super) fn decode(bytes: &[u8]) -> Result<Self, restate_sdk::errors::TerminalError> {
        let state: Self = crate::object_state::decode_stamped_bytes(
            super::TURN_OUTCOME_STATE,
            bytes,
            &super::TURN_OUTCOME_FORMATS,
        )?;
        if let Self::Ran { admitted, outcome } = &state
            && admitted.run() != outcome.run()
        {
            return Err(restate_sdk::errors::TerminalError::new(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::RuntimeStoreCorrupt,
                    "turn outcome names a different run than its recorded admission",
                )
                .to_record(),
            ));
        }
        Ok(state)
    }
}

/// The request `LashTurn/{session}:{request}#{ordinal}/close` runs: the scope close the
/// key's `run` owed once its run's terminal evidence was durable
/// (FIG-4035).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestateRunCloseRequest {
    /// The drain generation of the build whose run owed the close: a close
    /// sent on a generation lane names that lane's generation.
    #[serde(default)]
    pub sender_generation: Option<BuildGeneration>,
    /// The logical run whose scope closes: the key's own run, or, for a
    /// follow-on recovery, the run that owed the follow-on.
    pub run: lash_core::TurnId,
    /// The selected logical run that owns the close journal.
    pub scope_run: lash_core::TurnId,
}

/// The invocation of an immutable shift request and its admission ordinal.
#[must_use]
pub fn turn_invocation_key(request: &ShiftRequest, ordinal: u32) -> String {
    admission_invocation_key(
        &request.session,
        &lash_core::engine::AdmissionId::new(format!("{}#{ordinal}", request.request.as_str())),
    )
}

pub(crate) fn admission_invocation_key(
    session: &SessionId,
    admission: &lash_core::engine::AdmissionId,
) -> String {
    format!(
        "{}:{}{}",
        session.as_str().len(),
        session.as_str(),
        admission.as_str()
    )
}

/// Recover a run's invocation through its retained admission or seal.
pub async fn recorded_turn_invocation_key(
    stores: &dyn lash_core::store::RunStore,
    session: &SessionId,
    run: &lash_core::TurnId,
) -> Result<Option<String>, lash_core::StoreError> {
    Ok(match stores.run_executor(session, run).await? {
        Some(lash_core::store::RunExecutor::Run { admission }) => {
            Some(admission_invocation_key(session, &admission))
        }
        Some(
            lash_core::store::RunExecutor::Acceptor { .. }
            | lash_core::store::RunExecutor::Inline { .. },
        )
        | None => None,
    })
}

pub(crate) fn parse_turn_invocation_key(
    key: &str,
) -> Option<(SessionId, lash_core::engine::AdmissionId)> {
    let (len, rest) = key.split_once(':')?;
    if len.is_empty()
        || !len.bytes().all(|byte| byte.is_ascii_digit())
        || (len.len() > 1 && len.starts_with('0'))
    {
        return None;
    }
    let len = len.parse::<usize>().ok()?;
    if !rest.is_char_boundary(len) {
        return None;
    }
    let (session, admission) = rest.split_at(len);
    let (request, ordinal) = admission.rsplit_once('#')?;
    if request.is_empty() || ordinal.parse::<u32>().ok()?.to_string() != ordinal {
        return None;
    }
    Some((
        SessionId::parse(session).ok()?,
        lash_core::engine::AdmissionId::new(admission),
    ))
}

pub(super) async fn decode_run_intent(
    route: &str,
    generation: BuildGeneration,
    raw: serde_json::Value,
) -> restate_sdk::errors::HandlerResult<RestateRunRequest> {
    // Refuse pre-cutover input before decoding; remove at the 1.0 frozen-prefix reset.
    if raw.get("request").is_none() {
        let sentinel = crate::sentinel::FoldedSentinel::new(route, generation);
        sentinel
            .guard(sentinel.check(raw.get("sender_generation")))
            .await?;
    }
    serde_json::from_value(raw)
        .map_err(|error| super::misaddressed(format!("invalid turn intent: {error}")))
}

impl lash_core::store::DurableRecord for LashTurnState {
    const SURFACE: lash_core::store::SurfaceFormat =
        lash_core::surface_format!(crate::session_shifts::LASH_TURN_OUTCOME_FORMAT_VERSION);
}
