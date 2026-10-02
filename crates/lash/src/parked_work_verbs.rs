//! Operator verbs over the durable control-intent ledger.
use std::num::NonZeroUsize;
use std::sync::Arc;

use lash_core::store::{
    ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState, ParkId,
    RunIntentRefused, RunIntentRequest, RunTerminal, RunVerb,
};
use lash_sansio::{SessionId, TurnId};

use crate::parked_work::{ParkedWork, ParkedWorkRef};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParkVerbRefused {
    #[error("the run is not parked")]
    NotParked,
    #[error("the park was superseded by {current}")]
    ParkSuperseded { current: ParkId },
    #[error("the run is being redriven by {intent}")]
    Redriving { intent: ControlIntentId },
    #[error("intent {intent} is still open")]
    IntentOpen { intent: ControlIntentId },
    #[error("the session was deleted")]
    SessionDeleted,
    #[error("the session is closing")]
    SessionClosing,
    #[error(transparent)]
    Store(#[from] lash_core::StoreError),
    #[error("fork requires a turn run")]
    ForkRequiresTurn,
    #[error("{code}: {message}")]
    SubstrateRefused {
        code: lash_core::RuntimeErrorCode,
        message: String,
    },
}

impl From<RunIntentRefused> for ParkVerbRefused {
    fn from(error: RunIntentRefused) -> Self {
        match error {
            RunIntentRefused::NotParked => Self::NotParked,
            RunIntentRefused::ParkSuperseded { current } => Self::ParkSuperseded { current },
            RunIntentRefused::Redriving { intent } => Self::Redriving { intent },
            RunIntentRefused::IntentOpen { intent } => Self::IntentOpen { intent },
            RunIntentRefused::SessionDeleted => Self::SessionDeleted,
            RunIntentRefused::SessionClosing => Self::SessionClosing,
            RunIntentRefused::Store(error) => Self::Store(error),
            other => Self::SubstrateRefused {
                code: lash_core::RuntimeErrorCode::PluginSessionManager,
                message: other.to_string(),
            },
        }
    }
}

/// A run redrive committed to the ledger. The park stays until execution progresses.
#[derive(Clone, Debug)]
pub enum RedriveAccepted {
    Run(RunRedriveAccepted),
    Process {
        process: lash_sansio::ProcessId,
        park: ParkId,
    },
}

#[derive(Clone, Debug)]
pub struct RunRedriveAccepted {
    pub intent: ControlIntentId,
    pub applied: bool,
    pub run: TurnId,
}

#[derive(Clone, Debug)]
pub struct ParkCancelled {
    pub intent: ControlIntentId,
    pub terminal: RunTerminal,
    pub applied: bool,
}

#[derive(Clone, Debug)]
pub struct ForkedTurn {
    pub intent: ControlIntentId,
    pub cancelled: RunTerminal,
    pub new_run: Option<TurnId>,
    pub applied: bool,
}

#[derive(Clone, Debug)]
pub struct ControlIntentQuery {
    pub after: Option<ControlIntentId>,
    pub limit: NonZeroUsize,
}

#[derive(Clone, Debug)]
pub struct ControlIntentPage {
    pub intents: Vec<ControlIntent>,
    pub next: Option<ControlIntentId>,
}

impl ParkedWork {
    pub async fn redrive(
        &self,
        target: &ParkedWorkRef,
        park: ParkId,
    ) -> Result<RedriveAccepted, ParkVerbRefused> {
        let (session_id, turn_id) = match target {
            ParkedWorkRef::Turn {
                session_id,
                turn_id,
            } => (session_id, turn_id),
            ParkedWorkRef::Process { process_id } => {
                self.work
                    .ports()
                    .await
                    .queued_port()
                    .control()
                    .resume_process(process_id, park)
                    .await
                    .map_err(|refusal| ParkVerbRefused::SubstrateRefused {
                        code: refusal.code,
                        message: refusal.message,
                    })?;
                return Ok(RedriveAccepted::Process {
                    process: process_id.clone(),
                    park,
                });
            }
        };
        let (intent, applied) = self
            .run_verb(session_id, turn_id, park, RunVerb::Redrive)
            .await?;
        Ok(RedriveAccepted::Run(RunRedriveAccepted {
            intent: intent.id,
            applied,
            run: turn_id.clone(),
        }))
    }

    pub async fn cancel(
        &self,
        target: &ParkedWorkRef,
        park: ParkId,
    ) -> Result<ParkCancelled, ParkVerbRefused> {
        let ParkedWorkRef::Turn {
            session_id,
            turn_id,
        } = target
        else {
            return Err(ParkVerbRefused::SubstrateRefused {
                code: lash_core::RuntimeErrorCode::PluginSessionManager,
                message: "parked process cancellation is not installed".into(),
            });
        };
        let (intent, applied) = self
            .run_verb(session_id, turn_id, park, RunVerb::Cancel)
            .await?;
        Ok(ParkCancelled {
            intent: intent.id,
            terminal: self.terminal(session_id, turn_id).await?,
            applied,
        })
    }

    pub async fn fork(
        &self,
        session: &SessionId,
        run: &TurnId,
        park: ParkId,
    ) -> Result<ForkedTurn, ParkVerbRefused> {
        let (intent, applied) = self.run_verb(session, run, park, RunVerb::Fork).await?;
        let ControlIntentKind::Fork { new_run, .. } = intent.kind else {
            return Err(lash_core::StoreError::Contended.into());
        };
        Ok(ForkedTurn {
            intent: intent.id,
            cancelled: self.terminal(session, run).await?,
            new_run,
            applied,
        })
    }

    pub async fn intents(&self, query: &ControlIntentQuery) -> crate::Result<ControlIntentPage> {
        let mut intents = self
            .store_factory
            .list_control_intents(query.after, query.limit.saturating_add(1))
            .await?;
        let more = intents.len() > query.limit.get();
        intents.truncate(query.limit.get());
        let next = if more {
            intents.last().map(|intent| intent.id)
        } else {
            None
        };
        Ok(ControlIntentPage { intents, next })
    }

    async fn run_verb(
        &self,
        session: &SessionId,
        run: &TurnId,
        park: ParkId,
        verb: RunVerb,
    ) -> Result<(ControlIntent, bool), ParkVerbRefused> {
        let intent = self
            .store_factory
            .open_run_intent(
                &RunIntentRequest {
                    session_id: session.clone(),
                    run: run.clone(),
                    park,
                    verb,
                },
                self.clock.timestamp_ms(),
            )
            .await?;
        let state = lash_core::runtime::shift::ControlIntentRelay::new(
            Arc::clone(&self.intents),
            Arc::clone(&self.store_factory),
            self.work.ports().await.queued_port(),
            Arc::clone(&self.scopes),
            Arc::clone(&self.scope_close_obligations),
            Arc::clone(&self.clock),
        )
        .with_policy(self.relay_policy)
        .deliver_intent(&intent)
        .await?;
        Ok((
            intent,
            matches!(state, ControlIntentState::Acknowledged { .. }),
        ))
    }

    async fn terminal(
        &self,
        session: &SessionId,
        run: &TurnId,
    ) -> Result<RunTerminal, ParkVerbRefused> {
        self.store_factory
            .run_terminal(session, run)
            .await?
            .ok_or_else(|| lash_core::StoreError::Contended.into())
    }
}
