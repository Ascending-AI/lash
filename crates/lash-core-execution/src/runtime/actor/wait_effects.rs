//! The wait methods of the context (L5, FIG-5173): a durable sleep is a
//! timer wait row, and a session's or a closed run's waits are revoked by
//! scope in an owner transaction of the actor that owns them.

use lash_durable::CommitLabel;
use lash_durable::domain::{ScopeKey, WaitKind};

use super::ActorContext;
use super::waits::{self, RaceWinner, WaitDeadline, WaitSpec};
use crate::{RuntimeErrorCode, SleepSpec};

impl ActorContext {
    /// The wait effects: `Sleep` is a timer wait raced against cancel;
    /// `AwaitEvent` and `PeekAwaitEvent` are addressed by the retired
    /// await-event key and wait for their callers' ports. Any other command
    /// is refused.
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
            crate::RuntimeEffectCommand::AwaitEvent { key }
            | crate::RuntimeEffectCommand::PeekAwaitEvent { key } => {
                super::await_event_legacy::port_pending(&key.wait)
            }
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
        let now = self
            .backend()
            .durable()
            .now()
            .await
            .map_err(durable_refusal)?;
        let until = match spec {
            SleepSpec::For { duration_ms } => {
                now.after_millis(i64::try_from(duration_ms).unwrap_or(i64::MAX))
            }
            SleepSpec::Until { deadline_ms } => {
                lash_durable::DurableInstant(i64::try_from(deadline_ms).unwrap_or(i64::MAX))
            }
        };
        let mut tx = self.begin().await.map_err(durable_refusal)?;
        let (timer, _) = waits::pin(
            &mut tx,
            WaitSpec {
                kind: WaitKind::Timer,
                scope: waits::wait_scope(self),
                target_process: None,
                deadline: Some(WaitDeadline::at_instant(until)),
            },
        )
        .map_err(|refusal| {
            crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::InvalidAwaitEventWaitIdentity,
                refusal.to_string(),
            )
        })?;
        self.commit(tx, CommitLabel::WAIT_MINT)
            .await
            .map_err(durable_refusal)?;
        match waits::race(self, &[timer]).await.map_err(durable_refusal)? {
            RaceWinner::Resolved { .. } | RaceWinner::TimedOut(_) => {
                Ok(crate::RuntimeEffectOutcome::Sleep)
            }
            RaceWinner::Cancelled => Err(crate::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RuntimeEffectSleepCancelled,
                "the sleep's awaiter was cancelled",
            )),
        }
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

fn durable_refusal(error: lash_durable::DurableError) -> crate::RuntimeEffectControllerError {
    crate::RuntimeEffectControllerError::new(
        RuntimeErrorCode::EngineAwaitEventAwait,
        error.to_string(),
    )
}

fn revoke_refusal(error: lash_durable::DurableError) -> crate::RuntimeError {
    crate::RuntimeError::new(RuntimeErrorCode::EngineAwaitEventRevoke, error.to_string())
}
