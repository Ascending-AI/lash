//! Why a session actor parks, and its park (FIG-5230, FIG-5592, FIG-5401):
//! the typed reason the actor's row records, which an
//! operator reads (`LashSession::park_reason`) before redriving the session.

use lash_durable::{CommitLabel, DomainWrite, DurableError, Release};

use super::{TurnError, TurnRestoreError};
use crate::ActorContext;

/// Why a session actor parked: the actor row's typed reason. A parked
/// session runs nothing until an operator redrives it or a control wake (a
/// turn cancel) readies it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionParkReason {
    /// Its passes failed `failed_passes` times in a row within one claim,
    /// each with an error a retry does not clear: the activation-loop budget
    /// (FIG-5230).
    PassLoop {
        /// The passes in a row that failed.
        failed_passes: u32,
        /// The last pass's failure.
        error: SessionPassFailure,
    },
    /// Its activations ended `failed_activations` claims in a row with
    /// nothing committed (a panic, or a read the activation gave up on): the
    /// activation-loop budget, which the store counts across claims, as a
    /// process's (FIG-6096).
    ActivationLoop {
        /// The claims in a row that committed nothing.
        failed_activations: u32,
    },
    /// Its unfinished turn resumes from a state this build does not decode:
    /// another build wrote it. The turn's restore refused it before any of
    /// the turn's work was re-delivered (no cell ran again, no model call
    /// was resent), so nothing of the turn was committed or settled for it,
    /// and its rows keep the state: a build that reads it resumes the turn
    /// on a redrive, and a turn cancel ends it without reading it
    /// (FIG-5592, FIG-5601).
    UndecodableState {
        /// Which of the turn's states.
        state: ParkedTurnState,
        /// The decoder's account.
        message: String,
    },
}

/// The failure of a session's last pass before it parked: the typed runtime
/// error it failed with, its code and cause as the runtime raised them
/// (FIG-5401), so a park on a recorded model no deployment serves names its
/// `LlmProfileKey`. A failure of the session's own machinery (its store, its
/// mailbox, a restore) carries no runtime code, only its account.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPassFailure {
    /// The runtime error's code; `None` for a failure of the session's own
    /// machinery.
    pub code: Option<crate::RuntimeErrorCode>,
    /// The typed cause the runtime error carried beside its code.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::RuntimeErrorCause>,
    /// The failure's account.
    pub message: String,
}

impl SessionPassFailure {
    /// The failure `error` records.
    #[must_use]
    pub(crate) fn of(error: &TurnError) -> Self {
        match error {
            TurnError::Runtime(error) => Self {
                code: Some(error.code.clone()),
                cause: error.cause.clone(),
                message: error.message.clone(),
            },
            error => Self {
                code: None,
                cause: None,
                message: error.to_string(),
            },
        }
    }

    /// The recorded model key the pass could not bind, when that is why it
    /// failed.
    #[must_use]
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        crate::RuntimeErrorCause::profile_key(self.cause.as_ref())
    }
}

impl std::fmt::Display for SessionPassFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.code {
            Some(code) => write!(formatter, "{code}: {}", self.message),
            None => formatter.write_str(&self.message),
        }
    }
}

/// One of the states an unfinished turn resumes from.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ParkedTurnState {
    /// The phase checkpoint its row holds.
    Checkpoint,
    /// What its protocol driver parked in that checkpoint while a model
    /// call or a code cell ran.
    DriverState {
        /// The driver that refused it.
        driver: String,
    },
    /// The admission record of the model call it resends.
    AdmittedCall {
        /// The call's ordinal among the turn's calls.
        call: u32,
    },
    /// The snapshot the code cell it stopped in resumes from.
    CellSnapshot,
    /// A plugin namespace its run committed beside its checkpoint, which a
    /// resume reinstalls.
    PluginState,
    /// The working execution state its last consumed cell left, which a
    /// resume installs before the turn continues (FIG-5887 §6).
    ExecutionState,
}

/// The park of a pass that failed on a state its turn resumes from and this
/// build does not decode; `None` for every other failure.
pub(in crate::runtime::durable) fn undecodable_state(
    error: &TurnError,
) -> Option<SessionParkReason> {
    use lash_sansio::TurnCheckpointRestoreError as Checkpoint;
    let (state, reason) = match error {
        TurnError::UndecodableState { state, reason } => (state.clone(), reason.clone()),
        TurnError::Restore(TurnRestoreError::Checkpoint(Checkpoint::UndecodableDriverState(
            refusal,
        ))) => (
            ParkedTurnState::DriverState {
                driver: refusal.driver.clone(),
            },
            refusal.reason.clone(),
        ),
        TurnError::Restore(
            restore @ (TurnRestoreError::Undecodable { .. }
            | TurnRestoreError::Checkpoint(
                Checkpoint::IncompatibleSchemaVersion { .. }
                | Checkpoint::IncompatibleFormat { .. },
            )),
        ) => (ParkedTurnState::Checkpoint, restore.to_string()),
        _ => return None,
    };
    Some(SessionParkReason::UndecodableState {
        state,
        message: reason,
    })
}

impl SessionParkReason {
    /// The reason's kind: the serde `reason` tag's spelling, which a host
    /// reads without a serde round trip (FIG-6044). The park reports it as
    /// its `lash.parked_work.reason`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::PassLoop { .. } => "pass_loop",
            Self::ActivationLoop { .. } => "activation_loop",
            Self::UndecodableState { .. } => "undecodable_state",
        }
    }

    /// The recorded model key the session's passes could not bind, when
    /// that is why it parked.
    #[must_use]
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        match self {
            Self::PassLoop { error, .. } => error.profile_key(),
            Self::ActivationLoop { .. } | Self::UndecodableState { .. } => None,
        }
    }

    /// The actor row's park encoding.
    #[expect(
        clippy::expect_used,
        reason = "a park reason is plain data whose encoding cannot fail"
    )]
    #[must_use]
    pub fn encode(&self) -> String {
        serde_json::to_string(self).expect("a park reason encodes")
    }
}

/// Park the session with `reason`: its current reason and its release as
/// `parked`, one commit, counted as parked work on `tracing`'s metrics. Its
/// unread mail stays for the claim that follows a redrive.
pub(super) async fn park(
    cx: &ActorContext,
    reason: &SessionParkReason,
    tracing: Option<&crate::trace::TraceRuntime>,
) -> Result<(), DurableError> {
    tracing::warn!(actor = %cx.actor(), reason = %reason.encode(), "session parked");
    let mut tx = cx.begin().await?;
    tx.write(DomainWrite::ActorPark {
        reason_json: Some(reason.encode()),
    });
    tx.give_up(Release::Parked);
    cx.commit(tx, CommitLabel::SESSION_RELEASE).await?;
    if let Some(tracing) = tracing {
        crate::operational_metrics::record_work_parked(
            tracing.metrics(),
            Some(&lash_trace::EmissionPermit::new_transition()),
            "session",
            reason.kind(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `kind` is the serde `reason` tag's spelling for every variant, so a
    /// host stores the park kind without a serde round trip (FIG-6044).
    #[test]
    fn kind_is_the_serde_reason_tag() {
        let reasons = [
            SessionParkReason::PassLoop {
                failed_passes: 3,
                error: SessionPassFailure {
                    code: None,
                    cause: None,
                    message: "the pass failed".into(),
                },
            },
            SessionParkReason::ActivationLoop {
                failed_activations: 3,
            },
            SessionParkReason::UndecodableState {
                state: ParkedTurnState::Checkpoint,
                message: "this build does not decode it".into(),
            },
        ];
        for reason in &reasons {
            let tag = serde_json::to_value(reason).unwrap()["reason"].clone();
            assert_eq!(
                tag,
                serde_json::json!(reason.kind()),
                "{reason:?}: kind does not match its serde tag"
            );
        }
    }
}
