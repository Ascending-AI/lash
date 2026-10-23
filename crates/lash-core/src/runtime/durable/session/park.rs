//! Why a session actor parks, and its park (FIG-5230, FIG-5592, FIG-5401):
//! the typed reason the actor's row records, which an
//! operator reads (`LashSession::park_reason`) before redriving the session.
//!
//! A park never strands a stop (FIG-6099): an `Immediate` cancel of the
//! session's open turn, its close requested, or its close under way. A
//! stop the park's own read finds is run, not parked over, and one that
//! reaches the session after that read readies it as the park commits.
//! Only a stop that failed the activation-loop budget's claims in a row
//! parks, `CancelLoop`, and stays recorded: an operator's redrive, or a
//! further close request, runs it again.

use lash_core_execution::runtime::actor::activation_loop;
use lash_core_execution::runtime::turn_control::immediate_in;
use lash_durable::runner::{Exit, Owned};
use lash_durable::{ActorTx, ClaimPurpose, CommitLabel, DomainWrite, DurableError, Release};

use super::{TurnError, TurnRestoreError};
use crate::ActorContext;
use lash_vocabulary::identity::SessionId;

/// Why a session actor parked: the actor row's typed reason. A parked
/// session runs nothing until an operator redrives it, a control wake (a
/// turn cancel) readies it, or, for a park a change of build may clear
/// ([`Self::REDRIVEN_AT_BOOT`]), a node boots (FIG-6310).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case", deny_unknown_fields)]
pub enum SessionParkReason {
    /// A run still needs a registration the deployment retired. Nothing
    /// resumes or settles; a deployment restoring it may redrive the run.
    PluginRetired {
        refusal: crate::plugin::RetirementRefusal,
    },
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
    /// A stop of the session (an `Immediate` cancel of its open turn, or its
    /// close) failed every claim that ran it, `failed_activations` claims
    /// in a row with nothing committed: the activation-loop budget
    /// (FIG-6099). The stop stays recorded, and an operator's redrive or a
    /// further close request runs it again.
    CancelLoop {
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
/// `LlmProfileKey`. A window-restore wrapper preserves the runtime failure
/// beneath it, with the enclosing restore account. Other failures of the
/// session's own machinery carry only their account.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionPassFailure {
    /// The runtime error's code; `None` when there is no nested runtime failure.
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
            TurnError::Restore(TurnRestoreError::Window { source, .. }) => {
                let mut failure = Self::of(source);
                failure.message = error.to_string();
                failure
            }
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

/// A refusal that parks a recorded run before redelivery: state this build
/// cannot decode, or a registration it retired. `None` for other failures.
pub(in crate::runtime::durable) fn resume_refusal(error: &TurnError) -> Option<SessionParkReason> {
    if let TurnError::Runtime(error) = error
        && let Some(crate::RuntimeErrorCause::PluginRetired { refusal }) = &error.cause
    {
        return Some(SessionParkReason::PluginRetired {
            refusal: refusal.as_ref().clone(),
        });
    }
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
    /// The parks every node's boot redrives, by [`kind`](Self::kind)
    /// (FIG-6310): those another build may clear and whose every failure
    /// ended inside a live claim, so the node that met it outlived it. A
    /// [`PassLoop`](Self::PassLoop)'s passes each failed with a typed error,
    /// and an [`UndecodableState`](Self::UndecodableState)'s restore refused
    /// before any of the turn's work was delivered again. A retired plugin
    /// refusal also keeps the turn intact for a build that serves it. An
    /// [`ActivationLoop`](Self::ActivationLoop) or
    /// [`CancelLoop`](Self::CancelLoop) park counts claims that committed
    /// nothing, which a node's death also leaves: redriven at every boot, one
    /// such session could take down every node that boots, so it waits for
    /// an operator's redrive.
    pub const REDRIVEN_AT_BOOT: [&'static str; 3] =
        ["pass_loop", "undecodable_state", "plugin_retired"];

    /// The reason's kind: the serde `reason` tag's spelling, which a host
    /// reads without a serde round trip (FIG-6044). The park reports it as
    /// its `lash.parked_work.reason`.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::PluginRetired { .. } => "plugin_retired",
            Self::PassLoop { .. } => "pass_loop",
            Self::ActivationLoop { .. } => "activation_loop",
            Self::CancelLoop { .. } => "cancel_loop",
            Self::UndecodableState { .. } => "undecodable_state",
        }
    }

    /// The recorded model key the session's passes could not bind, when
    /// that is why it parked.
    #[must_use]
    pub fn profile_key(&self) -> Option<&crate::LlmProfileKey> {
        match self {
            Self::PassLoop { error, .. } => error.profile_key(),
            Self::ActivationLoop { .. }
            | Self::CancelLoop { .. }
            | Self::PluginRetired { .. }
            | Self::UndecodableState { .. } => None,
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

/// What a park of the session did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Parking {
    /// The park committed, and the actor is released: parked, or ready for
    /// a stop that reached it after the park's read.
    Committed,
    /// The park's read found a stop pending, and nothing committed: the
    /// stop runs before the session parks for anything else.
    Stopping,
}

/// Park the session with `reason`, unless the park's read finds a stop
/// pending ([`Parking::Stopping`]): its current reason and its release as
/// `parked`, one commit, counted as parked work on `tracing`'s metrics. Its
/// unread mail stays for the claim that follows a redrive.
pub(super) async fn park(
    cx: &ActorContext,
    session: &SessionId,
    reason: &SessionParkReason,
    tracing: Option<&crate::trace::TraceRuntime>,
) -> Result<Parking, DurableError> {
    let tx = cx.begin().await?;
    if stop_pending(cx, &tx, session).await? {
        return Ok(Parking::Stopping);
    }
    commit(cx, tx, reason, tracing).await?;
    Ok(Parking::Committed)
}

/// What a claim at the activation-loop budget leaves to run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Budget {
    /// The claim runs as any claim does.
    Within,
    /// The claim ends the pending stop engine-free, from the session's rows
    /// alone.
    Stop,
}

/// Check the activation-loop budget as the claim `owned` of `session`
/// reads it (FIG-6096, FIG-6099): the store counts the claims in a row
/// that ended with nothing committed. At the budget:
///
/// - with no stop pending, the session parks `ActivationLoop`;
/// - with a stop pending, the stop runs engine-free ([`Budget::Stop`]) on a
///   claim that runs the session, as a process's cancel ends it
///   engine-free: the failed claims were the session's own work;
/// - a stop that failed on its own claims (every claim of a node that only
///   ends it, or one more claim past the budget after its engine-free run)
///   parks `CancelLoop`, and stays recorded.
///
/// `Err` is the claim's exit, once it parked or lost the session.
pub(super) async fn at_claim(
    owned: &Owned,
    cx: &ActorContext,
    session: &SessionId,
    tracing: Option<&crate::trace::TraceRuntime>,
) -> Result<Budget, Exit> {
    let budget = cx.backend().config().settings().activation_loop_budget;
    let failed_activations = activation_loop::claimed_row(owned)
        .await?
        .failed_activations;
    if !activation_loop::reached(failed_activations, budget) {
        return Ok(Budget::Within);
    }
    let cancel_only = owned.purpose() == ClaimPurpose::CancelOnly;
    let stop_failed = cancel_only || failed_activations > budget;
    let parked = async {
        let tx = cx.begin().await?;
        let reason = match (stop_pending(cx, &tx, session).await?, stop_failed) {
            (true, true) => SessionParkReason::CancelLoop { failed_activations },
            (true, false) => return Ok(Some(Budget::Stop)),
            // A claim only for a stop that is no longer pending lets the
            // session go as any such claim does.
            (false, _) if cancel_only => return Ok(Some(Budget::Within)),
            (false, _) => SessionParkReason::ActivationLoop { failed_activations },
        };
        commit(cx, tx, &reason, tracing).await?;
        Ok::<_, DurableError>(None)
    };
    match parked.await {
        Ok(Some(budget)) => Ok(budget),
        Ok(None) | Err(DurableError::OwnershipLost(_)) => Err(Exit::Released),
        Err(_) => Err(Exit::Abandoned),
    }
}

/// Whether a stop of `session` is pending as of `tx`'s read: an `Immediate`
/// cancel its open turn accepted, its close requested, or its close under
/// way.
async fn stop_pending(
    cx: &ActorContext,
    tx: &ActorTx,
    session: &SessionId,
) -> Result<bool, DurableError> {
    Ok(immediate_in(tx)
        || cx
            .durable_reads()?
            .session_close(session)
            .await?
            .is_some_and(|close| !close.is_tombstone()))
}

/// Commit the session's park with `reason` on `tx`.
async fn commit(
    cx: &ActorContext,
    mut tx: ActorTx,
    reason: &SessionParkReason,
    tracing: Option<&crate::trace::TraceRuntime>,
) -> Result<(), DurableError> {
    tracing::warn!(actor = %cx.actor(), reason = %reason.encode(), "session parked");
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
            SessionParkReason::PluginRetired {
                refusal: crate::plugin::RetirementRefusal::HasState {
                    registry: lash_vocabulary::claims::RegistryKind::Session,
                    id: "retired".into(),
                },
            },
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
            SessionParkReason::CancelLoop {
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

    /// RLM-HISTORY-PIN: a cold restore's released base stays typed in the
    /// persisted park reason a host reads, with the enclosing restore account.
    #[test]
    fn a_released_turn_base_stays_typed_in_the_park_reason() {
        let error = TurnError::Restore(TurnRestoreError::Window {
            run: lash_vocabulary::identity::TurnId::from("released-base"),
            source: Box::new(TurnError::Runtime(
                crate::StoreError::TurnBaseNotRetained { revision: 17 }.runtime_error(),
            )),
        });
        let reason = SessionParkReason::PassLoop {
            failed_passes: 1,
            error: SessionPassFailure::of(&error),
        };
        let encoded = serde_json::to_value(reason).expect("park reason serializes");
        let decoded: SessionParkReason =
            serde_json::from_value(encoded).expect("park reason round-trips");
        let SessionParkReason::PassLoop { error: failure, .. } = decoded else {
            panic!("the park kind changed");
        };
        assert_eq!(failure.message, error.to_string());
        assert_eq!(failure.code, Some(crate::RuntimeErrorCode::StoreRefused));
        let Some(crate::RuntimeErrorCause::StoreRefusal { refusal }) = failure.cause else {
            panic!("the park lost the released base's structured cause");
        };
        assert!(matches!(
            refusal.into_store_error(),
            crate::StoreError::TurnBaseNotRetained { revision: 17 }
        ));
    }
}
