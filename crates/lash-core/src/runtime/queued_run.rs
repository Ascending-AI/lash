use super::*;

/// The factory stays unbound until persistence admits the queued run.
#[derive(Clone)]
pub enum QueuedEffectSource<'a> {
    Host {
        host: &'a dyn crate::EffectHost,
        identity: Option<crate::ExecutionScope>,
    },
    Controller {
        controller: &'a dyn crate::RuntimeEffectController,
        identity: Option<crate::ExecutionScope>,
    },
    Scoped(ScopedEffectController<'a>),
}

impl<'a> QueuedEffectSource<'a> {
    pub(crate) fn identity(&self) -> Option<crate::ExecutionScope> {
        match self {
            Self::Host { identity, .. } | Self::Controller { identity, .. } => identity.clone(),
            Self::Scoped(scoped) => Some(scoped.execution_scope().clone()),
        }
    }

    pub(crate) async fn acquire_lane(
        &self,
        lane: Arc<dyn crate::QueuedLaneProbe>,
        cancel: CancellationToken,
    ) -> Result<crate::QueuedLaneAcquisition, RuntimeError> {
        match self {
            Self::Host { host, .. } => host.acquire_queued_lane(lane, cancel).await,
            Self::Controller { controller, .. } => {
                controller.acquire_queued_lane(lane, cancel).await
            }
            Self::Scoped(scoped) => scoped.controller().acquire_queued_lane(lane, cancel).await,
        }
    }

    fn scoped(
        &self,
        scope: crate::ExecutionScope,
    ) -> Result<ScopedEffectController<'a>, RuntimeError> {
        let admitted = crate::AdmittedScope::unpinned(scope)?;
        match self {
            Self::Host { host, .. } => host.scoped(admitted),
            Self::Controller { controller, .. } => {
                ScopedEffectController::borrowed(*controller, admitted)
            }
            Self::Scoped(scoped) if scoped.execution_scope() == admitted.scope() => {
                Ok(scoped.clone())
            }
            Self::Scoped(_) => Err(RuntimeError::new(
                RuntimeErrorCode::ExecutionScopeTurnIdMismatch,
                "queued admission differs from supplied effect scope",
            )),
        }
    }
}

pub struct QueuedTurnOptions<'a> {
    pub(crate) source: QueuedEffectSource<'a>,
    pub(crate) cancel: CancellationToken,
    events: Option<&'a dyn EventSink>,
    turn_events: Option<&'a dyn TurnActivitySink>,
    local_cancel_origin: Option<TurnCancelOriginHint>,
}

impl<'a> QueuedTurnOptions<'a> {
    pub fn new(cancel: CancellationToken, source: QueuedEffectSource<'a>) -> Self {
        Self {
            source,
            cancel,
            events: None,
            turn_events: None,
            local_cancel_origin: None,
        }
    }
    pub fn with_events(mut self, events: &'a dyn EventSink) -> Self {
        self.events = Some(events);
        self
    }
    pub fn with_turn_events(mut self, events: &'a dyn TurnActivitySink) -> Self {
        self.turn_events = Some(events);
        self
    }
    pub fn with_local_cancel_origin_hint(mut self, hint: TurnCancelOriginHint) -> Self {
        self.local_cancel_origin = Some(hint);
        self
    }
    pub(crate) fn bind(
        &self,
        scope: crate::ExecutionScope,
    ) -> Result<TurnOptions<'a>, RuntimeError> {
        Ok(TurnOptions {
            scoped_effect_controller: self.source.scoped(scope)?,
            cancel: self.cancel.clone(),
            events: self.events,
            turn_events: self.turn_events,
            local_cancel_origin: self.local_cancel_origin.clone(),
        })
    }
}

impl<'a> From<TurnOptions<'a>> for QueuedTurnOptions<'a> {
    fn from(options: TurnOptions<'a>) -> Self {
        Self {
            source: QueuedEffectSource::Scoped(options.scoped_effect_controller),
            cancel: options.cancel,
            events: options.events,
            turn_events: options.turn_events,
            local_cancel_origin: options.local_cancel_origin,
        }
    }
}
