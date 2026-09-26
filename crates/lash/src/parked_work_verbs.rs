//! Operator verbs over the durable control-intent ledger.
use std::num::NonZeroUsize;

use lash_core::store::{
    ControlIntent, ControlIntentId, ControlIntentKind, ControlIntentState, ParkId,
    RootIntentRefused, RootIntentRequest, RootTerminal, RootVerb,
};
use lash_sansio::{SessionId, TurnId};

use crate::parked_work::{ParkedWork, ParkedWorkRef};

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ParkVerbRefused {
    #[error("the root is not parked")]
    NotParked,
    #[error("the park was superseded by {current}")]
    ParkSuperseded { current: ParkId },
    #[error("the root is being redriven by {intent}")]
    Redriving { intent: ControlIntentId },
    #[error("intent {intent} is still open")]
    IntentOpen { intent: ControlIntentId },
    #[error("the session was deleted")]
    SessionDeleted,
    #[error("the session is closing")]
    SessionClosing,
    #[error("the root owns {count} open effect groups")]
    EffectGroupsOpen { count: usize },
    #[error(transparent)]
    Store(#[from] lash_core::StoreError),
    #[error("fork requires a turn root")]
    ForkRequiresTurn,
    #[error("{code}: {message}")]
    SubstrateRefused {
        code: lash_core::RuntimeErrorCode,
        message: String,
    },
}

impl From<RootIntentRefused> for ParkVerbRefused {
    fn from(error: RootIntentRefused) -> Self {
        match error {
            RootIntentRefused::NotParked => Self::NotParked,
            RootIntentRefused::ParkSuperseded { current } => Self::ParkSuperseded { current },
            RootIntentRefused::Redriving { intent } => Self::Redriving { intent },
            RootIntentRefused::IntentOpen { intent } => Self::IntentOpen { intent },
            RootIntentRefused::SessionDeleted => Self::SessionDeleted,
            RootIntentRefused::SessionClosing => Self::SessionClosing,
            RootIntentRefused::EffectGroupsOpen { count } => Self::EffectGroupsOpen { count },
            RootIntentRefused::Store(error) => Self::Store(error),
            other => Self::SubstrateRefused {
                code: lash_core::RuntimeErrorCode::PluginSessionManager,
                message: other.to_string(),
            },
        }
    }
}

/// A root redrive committed to the ledger. The park stays until execution progresses.
#[derive(Clone, Debug)]
pub enum RedriveAccepted {
    Root(RootRedriveAccepted),
    Process {
        process: lash_sansio::ProcessId,
        park: ParkId,
    },
}

#[derive(Clone, Debug)]
pub struct RootRedriveAccepted {
    pub intent: ControlIntentId,
    pub applied: bool,
    pub root: TurnId,
}

#[derive(Clone, Debug)]
pub struct ParkCancelled {
    pub intent: ControlIntentId,
    pub terminal: RootTerminal,
    pub applied: bool,
}

#[derive(Clone, Debug)]
pub struct ForkedTurn {
    pub intent: ControlIntentId,
    pub cancelled: RootTerminal,
    pub new_root: Option<TurnId>,
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
                    .control()
                    .resume_process(process_id, park)
                    .await
                    .map_err(|error| {
                        let code = match &error {
                            lash_core::engine::EngineRefusal::Permanent { code, .. } => {
                                code.clone()
                            }
                            _ => lash_core::RuntimeErrorCode::PluginSessionManager,
                        };
                        ParkVerbRefused::SubstrateRefused {
                            code,
                            message: error.to_string(),
                        }
                    })?;
                return Ok(RedriveAccepted::Process {
                    process: process_id.clone(),
                    park,
                });
            }
        };
        let (intent, applied) = self
            .root_verb(session_id, turn_id, park, RootVerb::Redrive)
            .await?;
        Ok(RedriveAccepted::Root(RootRedriveAccepted {
            intent: intent.id,
            applied,
            root: turn_id.clone(),
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
            .root_verb(session_id, turn_id, park, RootVerb::Cancel)
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
        root: &TurnId,
        park: ParkId,
    ) -> Result<ForkedTurn, ParkVerbRefused> {
        let (intent, applied) = self.root_verb(session, root, park, RootVerb::Fork).await?;
        let ControlIntentKind::Fork { new_root, .. } = intent.kind else {
            return Err(lash_core::StoreError::Contended.into());
        };
        Ok(ForkedTurn {
            intent: intent.id,
            cancelled: self.terminal(session, root).await?,
            new_root,
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

    async fn root_verb(
        &self,
        session: &SessionId,
        root: &TurnId,
        park: ParkId,
        verb: RootVerb,
    ) -> Result<(ControlIntent, bool), ParkVerbRefused> {
        let intent = self
            .store_factory
            .open_root_intent(
                &RootIntentRequest {
                    session_id: session.clone(),
                    root: root.clone(),
                    park,
                    verb,
                },
                self.clock.timestamp_ms(),
            )
            .await?;
        let state = lash_core::runtime::drive::apply_control_intent(
            self.store_factory.as_ref(),
            self.work.control().as_ref(),
            self.work.as_ref(),
            self.scopes.as_ref(),
            &intent,
            self.clock.as_ref(),
        )
        .await?;
        Ok((
            intent,
            matches!(state, ControlIntentState::Acknowledged { .. }),
        ))
    }

    async fn terminal(
        &self,
        session: &SessionId,
        root: &TurnId,
    ) -> Result<RootTerminal, ParkVerbRefused> {
        self.store_factory
            .root_terminal(session, root)
            .await?
            .ok_or_else(|| lash_core::StoreError::Contended.into())
    }
}
