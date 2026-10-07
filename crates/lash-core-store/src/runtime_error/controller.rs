//! Effect-controller errors, journal policy and runtime conversions.

use super::*;

#[derive(Clone, Debug, thiserror::Error, Serialize, Deserialize)]
#[error("{code}: {message}")]
pub struct RuntimeEffectControllerError {
    #[serde(skip)]
    pub(super) journal_disposition: EffectErrorJournalPolicy,
    pub code: RuntimeErrorCode,
    pub message: String,
    /// Boxed, as on [`RuntimeError`]: the rare diagnostic must not size
    /// every effect outcome that carries an error inline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<Box<crate::RuntimeEffectReplayMismatchReport>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<crate::RuntimeErrorCause>,
    /// `true` when this error is the journaled record of a failed effect —
    /// decoded out of a settled `Failed` terminal — rather than a live fault
    /// that left nothing recorded. Consumers keep a journaled error on the
    /// result surface because it replays identically on every redrive; a
    /// live fault instead aborts like a crash so recovery can re-run the
    /// attempt (FIG-3528). Never persisted: the flag is stamped at the
    /// journal's replay boundary, not read back out of the row.
    #[serde(skip)]
    pub journaled: bool,
    /// The cause class the minting host chose for a foreign code (FIG-3575).
    /// Never persisted, like `journaled`.
    #[serde(skip)]
    pub(super) foreign_cause: Option<TurnFailureCause>,
}

impl RuntimeEffectControllerError {
    pub fn new(code: RuntimeErrorCode, message: impl Into<String>) -> Self {
        Self {
            journal_disposition: EffectErrorJournalPolicy::Terminal,
            code,
            message: message.into(),
            summary: None,
            cause: None,
            journaled: false,
            foreign_cause: None,
        }
    }

    /// A durable record cannot be decoded. Keeps its kind and codec diagnostic
    /// typed through the journal, plugin and runtime boundaries.
    #[must_use]
    pub fn stored_data_corrupt(record_kind: impl Into<String>, message: impl Into<String>) -> Self {
        let record_kind = record_kind.into();
        let message = message.into();
        let mut error = Self::new(
            RuntimeErrorCode::RuntimeStoreCorrupt,
            format!("stored {record_kind} data is corrupt: {message}"),
        );
        error.cause = Some(RuntimeErrorCause::StoredDataCorrupt {
            corruption: Box::new(StoredDataCorruption {
                record_kind,
                message,
            }),
        });
        error
    }

    /// The fenced referrer an `ArtifactReferrerEnded` refusal names. `None`
    /// on any other error.
    #[must_use]
    pub fn ended_referrer(&self) -> Option<&crate::artifact_referrer::ArtifactReferrer> {
        RuntimeErrorCause::ended_referrer(self.cause.as_ref())
    }

    /// Marks a failed, uncommitted host response derivation as safe to execute again.
    /// This authority is local to the executor; decoding a stored error cannot mint it.
    pub fn retryable_response_derivation(message: impl Into<String>) -> Self {
        let mut error = Self::new(
            RuntimeErrorCode::RuntimeEffectAssistantResponseHook,
            message,
        );
        error.journal_disposition = EffectErrorJournalPolicy::RetryUncommittedResponseDerivation;
        error
    }

    /// Whether this failure is the attempt's own: an uncommitted derivation
    /// marked safe to execute again ([`Self::retryable_response_derivation`],
    /// [`Self::retryable_uncommitted_derivation`]). The retry runs the failed
    /// work again, so nothing may be journaled after it in its place — a
    /// cell that failed on its host's worker verdict records no cancellation
    /// peek either (FIG-4451).
    pub fn is_attempt_fault(&self) -> bool {
        !self.is_terminal() && self.journal_disposition.is_retryable_derivation()
    }

    /// Marks this failure of an uncommitted host derivation — an
    /// execution-environment sync's rebuild, or a recorded execution-environment
    /// load, or a presentation whose recorded renderer is unavailable — as
    /// safe to execute again. The claim is released unsealed instead of
    /// journaling the failure as the effect's outcome (FIG-3587, FIG-3683).
    ///
    /// A terminal cause or code never takes this authority (FIG-4629).
    /// Like session retirement (FIG-3630), it is a settled refusal, so the
    /// step records it and every replay decodes the same answer.
    #[must_use]
    pub fn retryable_uncommitted_derivation(mut self) -> Self {
        self.journal_disposition = if self.is_terminal() {
            EffectErrorJournalPolicy::Terminal
        } else {
            EffectErrorJournalPolicy::RetryUncommittedResponseDerivation
        };
        self
    }

    /// A step whose execution lost its watch on the turn's cancellation gate
    /// (FIG-3672 P9): the watch retried and gave up, so the attempt ends with
    /// this live fault instead of a recorded outcome. It is never journaled
    /// and never read as a cancellation — the engine runs the step again.
    pub fn turn_cancel_watch_lost(message: impl Into<String>) -> Self {
        Self::new(RuntimeErrorCode::TransientCancelWatch, message)
            .retryable_uncommitted_derivation()
    }

    /// Only the host derivations — the before-LLM-call and assistant-response hooks,
    /// execution-environment sync, a checkpoint's store admission (FIG-4651),
    /// execution-environment load, presentation
    /// whose recorded renderer is unavailable, and a presentation or language
    /// value whose output retention faulted (FIG-1643) — and a
    /// shift's admission, a run's resolution (its spec read and its
    /// definition lookup, FIG-3838), a config transaction's resolution under
    /// reducers other than it was admitted with (FIG-4379), a run's scope
    /// close and a session's close,
    /// whose store faults are the attempt's (FIG-3600), a trigger delivery's
    /// admission, whose binding read is the attempt's (FIG-4369), a follow-on
    /// recovery run's decision (FIG-4361), and a process command
    /// that marked its registry fault retryable (a session deletion's process
    /// cleanup, after its close) can consume derivation retry authority, as
    /// can any step whose cancellation watch was lost
    /// ([`Self::turn_cancel_watch_lost`]): that fault is about the attempt,
    /// never the step. A model call and a direct completion consume it for
    /// one more fault alone: the recorded model this worker could not bind
    /// before the call ([`Self::llm_profile_unavailable`], FIG-4404). A tool attempt
    /// also consumes local retry authority for a typed VM worker fault
    /// encountered before its pure derivation returned (FIG-4707). Any step
    /// consumes it for a fenced plugin-state publication (FIG-4878): the
    /// body's result was never refused, only left unpublishable here.
    pub fn journal_disposition(&self, kind: RuntimeEffectKind) -> EffectErrorJournalPolicy {
        // The public cause can be attached after retry authority was granted.
        // No effect kind may consume that authority for a terminal refusal.
        if self.is_terminal() {
            return EffectErrorJournalPolicy::Terminal;
        }
        if matches!(
            kind,
            RuntimeEffectKind::BeforeLlmCall
                | RuntimeEffectKind::AssistantResponseHooks
                | RuntimeEffectKind::SyncExecutionEnvironment
                | RuntimeEffectKind::Checkpoint
                | RuntimeEffectKind::LoadExecutionEnv
                | RuntimeEffectKind::PresentToolResult
                | RuntimeEffectKind::LanguageRuntimeValue
                | RuntimeEffectKind::AdmitShift
                | RuntimeEffectKind::TransitionPlugins
                | RuntimeEffectKind::RecoverFollowOn
                | RuntimeEffectKind::ResolveTurnConfig
                | RuntimeEffectKind::ResolveConfigTransaction
                | RuntimeEffectKind::CloseRunScope
                | RuntimeEffectKind::IngestTriggerOccurrence
                | RuntimeEffectKind::AdmitTriggerDelivery
                | RuntimeEffectKind::Process
        ) || self.code == RuntimeErrorCode::TransientCancelWatch
            || self.is_unbound_llm_profile_call(kind)
            || matches!(
                (kind, self.cause.as_ref()),
                (
                    RuntimeEffectKind::ToolAttempt,
                    Some(RuntimeErrorCause::VmWorker { .. })
                )
            )
            || matches!(
                self.cause,
                Some(RuntimeErrorCause::PluginStatePublicationFenced { .. })
            )
        {
            self.journal_disposition
        } else {
            EffectErrorJournalPolicy::Terminal
        }
    }

    /// Hosts must namespace these codes and must not mint a built-in
    /// [`RuntimeErrorCode`] spelling. First-party producers use [`Self::new`],
    /// whose typed argument makes an unclassified string a compile error.
    pub fn foreign(
        code: impl Into<String>,
        cause: TurnFailureCause,
        message: impl Into<String>,
    ) -> Self {
        let mut error = Self::new(RuntimeErrorCode::ForeignCode(code.into()), message);
        error.foreign_cause = Some(cause);
        error
    }

    /// Marks this error as the journaled record of a failed effect — the
    /// `Failed` terminal a settled row replays — so consumers surface it as
    /// the attempt's recorded outcome instead of treating it as a live
    /// journal fault.
    pub fn into_journaled(mut self) -> Self {
        self.journaled = true;
        self
    }

    /// Whether this is a session-retirement refusal: the session was deleted,
    /// or is being deleted, under the turn it failed (FIG-3630).
    ///
    /// Its class is unchanged (terminal, never retried), but a turn failing
    /// with it aborts rather than recording a failed turn: the retirement owns
    /// the turn and leaves no head to record on. Recording would authorize a
    /// cancellation-closure pin the revoked session can never settle, and
    /// that pin refuses the deletion itself.
    pub fn is_session_retirement(&self) -> bool {
        matches!(self.cause, Some(RuntimeErrorCause::SessionDeleted { .. }))
    }

    /// The store refusal this error carries
    /// ([`RuntimeError::store_refusal`]).
    pub fn store_refusal(&self) -> Option<&crate::store::StoreRefusal> {
        match self.cause.as_ref()? {
            RuntimeErrorCause::StoreRefusal { refusal } => Some(refusal),
            _ => None,
        }
    }

    /// Whether retrying cannot succeed without a host-side change: a terminal
    /// cause, a foreign code its host minted as an outcome, or a terminal
    /// code.
    pub fn is_terminal(&self) -> bool {
        if let Some(class) = self
            .cause
            .as_ref()
            .and_then(RuntimeErrorCause::plugin_failure_class)
        {
            return class == lash_sansio::PluginFailureClass::Terminal;
        }
        self.has_terminal_cause()
            || match self.foreign_cause {
                Some(cause) => cause == TurnFailureCause::Outcome,
                None => self.code.is_terminal(),
            }
    }

    /// The cause class of a turn this error fails (FIG-3575).
    ///
    /// A journaled error is the recorded outcome of its effect whatever its
    /// code (FIG-3528: journaled outcomes stay on the result surface), so a
    /// redrive replays it as the same recorded failure instead of aborting on
    /// it forever. Any other error is an outcome exactly when it is terminal.
    pub fn turn_failure_cause(&self) -> TurnFailureCause {
        if self.journaled || self.has_terminal_cause() {
            return TurnFailureCause::Outcome;
        }
        if let Some(class) = self
            .cause
            .as_ref()
            .and_then(RuntimeErrorCause::plugin_failure_class)
        {
            return match class {
                lash_sansio::PluginFailureClass::Terminal => TurnFailureCause::Outcome,
                lash_sansio::PluginFailureClass::Parked => TurnFailureCause::Parked,
                _ => TurnFailureCause::LiveFault,
            };
        }
        match self.foreign_cause {
            Some(cause) => cause,
            None => self.code.turn_failure_cause(),
        }
    }

    /// Sets the summary carried by a `RuntimeEffectControllerError` for effect-host implementors
    /// while executing or replaying a runtime effect.
    pub fn with_summary(mut self, summary: crate::RuntimeEffectReplayMismatchReport) -> Self {
        self.summary = Some(Box::new(summary));
        self
    }

    pub fn wrong_outcome(expected: RuntimeEffectKind, actual: RuntimeEffectKind) -> Self {
        Self::new(
            RuntimeErrorCode::RuntimeEffectWrongOutcome,
            format!(
                "expected {} outcome, got {}",
                expected.as_str(),
                actual.as_str()
            ),
        )
    }

    pub fn into_runtime_error(self) -> RuntimeError {
        let Self {
            journal_disposition: _,
            code,
            message,
            summary,
            cause,
            journaled: _,
            foreign_cause,
        } = self;
        let mut runtime = RuntimeError::new(code, message);
        runtime.summary = summary;
        runtime.foreign_cause = foreign_cause;
        match cause {
            Some(cause) => runtime.with_cause(cause),
            None => runtime,
        }
    }
}

impl From<RuntimeError> for RuntimeEffectControllerError {
    fn from(err: RuntimeError) -> Self {
        Self {
            journal_disposition: EffectErrorJournalPolicy::Terminal,
            code: err.code,
            message: err.message,
            summary: err.summary,
            cause: err.cause,
            journaled: false,
            foreign_cause: err.foreign_cause,
        }
    }
}

impl From<lash_sansio::EffectIdentityError> for RuntimeEffectControllerError {
    fn from(error: lash_sansio::EffectIdentityError) -> Self {
        RuntimeError::from(error).into()
    }
}

/// Blank text offered as an identity is the store's refusal of it, with the
/// class the store gives that refusal.
impl From<crate::BlankIdentity> for RuntimeError {
    fn from(error: crate::BlankIdentity) -> Self {
        crate::StoreError::from(error).runtime_error()
    }
}

impl From<crate::BlankIdentity> for RuntimeEffectControllerError {
    fn from(error: crate::BlankIdentity) -> Self {
        crate::StoreError::from(error).into()
    }
}

impl From<crate::StoreError> for RuntimeEffectControllerError {
    fn from(err: crate::StoreError) -> Self {
        Self::from(&err)
    }
}

impl From<&crate::StoreError> for RuntimeEffectControllerError {
    fn from(err: &crate::StoreError) -> Self {
        err.runtime_error().into()
    }
}
