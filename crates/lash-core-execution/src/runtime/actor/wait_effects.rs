//! The wait methods of the context (L5, FIG-5173): a durable sleep is a
//! timer wait row, and a session's or a closed run's waits are revoked by
//! scope in an owner transaction of the actor that owns them.

use lash_durable::CommitLabel;
use lash_durable::domain::{ScopeKey, WaitKind};

use super::ActorContext;
use super::waits::{self, RaceWinner, WaitDeadline, WaitRef, WaitSpec};
use crate::{RuntimeErrorCode, SleepSpec};

impl ActorContext {
    /// The wait effects: `Sleep` is a timer wait raced against cancel. Any
    /// other command is refused.
    ///
    /// # Errors
    ///
    /// The effect's refusal.
    pub async fn wait_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        _local: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        match envelope.command {
            crate::RuntimeEffectCommand::Sleep { spec } => self.sleep(spec).await,
            other => Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::AwaitEventUnsupported,
                format!("{:?} is not a wait effect", other.kind()),
            )),
        }
    }

    /// A durable sleep: a timer wait row whose deadline is the sleep's end,
    /// pinned (`wait.mint`) and raced against the awaiter's cancel mail.
    async fn sleep(
        &self,
        spec: SleepSpec,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        let spec = timer(self, spec).await?;
        let mut tx = self.begin().await.map_err(durable_refusal)?;
        let (timer, _) = waits::pin(&mut tx, spec).map_err(|refusal| {
            crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::InvalidAwaitEventWaitIdentity,
                refusal.to_string(),
            )
        })?;
        self.commit(tx, CommitLabel::WAIT_MINT)
            .await
            .map_err(durable_refusal)?;
        race_timer(self, timer)
            .await
            .map(|()| crate::RuntimeEffectOutcome::Sleep)
    }

    /// Revoke a session's waits: its session scope and every scope of the
    /// session actor's pending waits, in one owner transaction
    /// (`wait.revoke`) of the session's actor.
    ///
    /// # Errors
    ///
    /// The store's refusal; this context must own the session's actor.
    pub async fn revoke_await_events_for_session(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<(), crate::RuntimeError> {
        let actor = lash_durable::ActorKey::session(session_id.as_str()).map_err(|error| {
            crate::RuntimeError::new(RuntimeErrorCode::EngineAwaitEventRevoke, error.to_string())
        })?;
        let pending = self
            .backend()
            .durable()
            .pending_waits(&actor)
            .await
            .map_err(revoke_refusal)?;
        let mut scopes: Vec<ScopeKey> = pending.into_iter().map(|row| row.scope).collect();
        scopes.push(ScopeKey::Session(session_id.clone()));
        self.revoke_scopes(scopes).await
    }

    /// Cancel a session's waits: they are revoked, as by
    /// [`Self::revoke_await_events_for_session`].
    ///
    /// # Errors
    ///
    /// The store's refusal; this context must own the session's actor.
    pub async fn cancel_await_events_for_session(
        &self,
        session_id: &crate::SessionId,
    ) -> Result<(), crate::RuntimeError> {
        self.revoke_await_events_for_session(session_id).await
    }

    /// Revoke a closed run's waits: its turn scope, and the committed
    /// turn's when another turn committed for it.
    ///
    /// # Errors
    ///
    /// The store's refusal; this context must own the session's actor.
    pub async fn retire_closed_run_waits(
        &self,
        session_id: &crate::SessionId,
        run: &crate::TurnId,
        committed_turn: Option<&crate::TurnId>,
    ) -> Result<(), crate::RuntimeError> {
        let mut scopes = vec![ScopeKey::Turn(session_id.clone(), run.clone())];
        if let Some(turn) = committed_turn.filter(|turn| *turn != run) {
            scopes.push(ScopeKey::Turn(session_id.clone(), turn.clone()));
        }
        self.revoke_scopes(scopes).await
    }

    async fn revoke_scopes(&self, mut scopes: Vec<ScopeKey>) -> Result<(), crate::RuntimeError> {
        scopes.sort();
        scopes.dedup();
        let mut tx = self.begin().await.map_err(revoke_refusal)?;
        for scope in &scopes {
            waits::revoke_scope(&mut tx, scope);
        }
        self.commit(tx, CommitLabel::WAIT_REVOKE)
            .await
            .map(|_| ())
            .map_err(revoke_refusal)
    }
}

/// The timer wait a durable sleep of `spec` pins under `cx`: due at the
/// sleep's end, dated on the store's clock now and never again, and revoked
/// with `cx`'s execution. A caller that pins it with a snapshot (a code
/// cell's quiet point) races the same row after a restore.
///
/// # Errors
///
/// The store's refusal to read its clock.
pub async fn timer(
    cx: &ActorContext,
    spec: SleepSpec,
) -> Result<WaitSpec, crate::RuntimeEffectControllerError> {
    let until = match spec {
        SleepSpec::For { duration_ms } => cx
            .backend()
            .durable()
            .now()
            .await
            .map_err(durable_refusal)?
            .after_millis(i64::try_from(duration_ms).unwrap_or(i64::MAX)),
        SleepSpec::Until { deadline_ms } => {
            lash_durable::DurableInstant(i64::try_from(deadline_ms).unwrap_or(i64::MAX))
        }
    };
    Ok(WaitSpec {
        kind: WaitKind::Timer,
        scope: waits::wait_scope(cx).map_err(durable_refusal)?,
        target_process: None,
        deadline: Some(WaitDeadline::at_instant(until)),
    })
}

/// Races the pinned timer `timer` against `cx`'s cancel mail: the sleep
/// ends at the deadline the timer was pinned with, however often it is
/// raced again.
///
/// # Errors
///
/// `RuntimeEffectSleepCancelled` when the awaiter was cancelled first, or
/// the store's refusal.
pub async fn race_timer(
    cx: &ActorContext,
    timer: WaitRef,
) -> Result<(), crate::RuntimeEffectControllerError> {
    match waits::race(cx, &[timer]).await.map_err(durable_refusal)? {
        RaceWinner::Resolved { .. } | RaceWinner::TimedOut(_) => Ok(()),
        RaceWinner::Cancelled => Err(crate::RuntimeEffectControllerError::new(
            RuntimeErrorCode::RuntimeEffectSleepCancelled,
            "the sleep's awaiter was cancelled",
        )),
    }
}

/// Sleeps `ctx`'s execution until the deadline of `timer`, the timer wait a
/// code cell's quiet point pinned with its sleep's admission (ADR 0132 §6,
/// §8): a restored cell races the same row, so its sleep keeps the deadline
/// it was admitted with.
///
/// # Errors
///
/// The race's refusal. A retryable one is recorded as the execution's
/// nested error, and a sleep its turn's cancellation won marks the turn
/// cancelled.
pub async fn sleep_until_timer(
    ctx: &crate::RuntimeExecutionContext<'_>,
    timer: WaitRef,
) -> Result<(), crate::RuntimeEffectControllerError> {
    race_timer(ctx.actor_context(), timer)
        .await
        .inspect_err(|error| {
            // A retryable sleep failure is the host asking for redelivery,
            // not a guest-visible sleep result. Raise it at the handler
            // boundary so the guest cannot swallow it into a terminal
            // process failure (FIG-3149).
            if error.code.is_retryable() {
                ctx.record_nested_effect_error(error.clone());
            }
            // A sleep that lost to the turn's cancellation gate is a
            // recorded outcome: the turn is cancelled from here on.
            if error.code == RuntimeErrorCode::RuntimeEffectSleepCancelled
                && ctx
                    .turn_cancel_wait(tokio_util::sync::CancellationToken::new())
                    .observes_turn_cancel()
            {
                ctx.note_turn_cancelled();
            }
        })
}

fn durable_refusal(error: lash_durable::DurableError) -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        RuntimeErrorCode::EngineAwaitEventAwait,
        error.to_string(),
    )
}

fn revoke_refusal(error: lash_durable::DurableError) -> crate::RuntimeError {
    crate::RuntimeError::new(RuntimeErrorCode::EngineAwaitEventRevoke, error.to_string())
}
