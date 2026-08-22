#[derive(Clone)]
pub struct ProcessOpScope<'scope> {
    pub(crate) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(crate) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'scope>,
    pub(crate) agent_frame_id: Option<crate::AgentFrameId>,
    pub(crate) turn_cancellation: Option<crate::ProcessTurnCancellation>,
}

impl<'scope> ProcessOpScope<'scope> {
    /// Constructs a `ProcessOpScope` for store and durable-substrate implementors while persisting
    /// and coordinating durable process execution.
    pub fn new(scoped_effect_controller: crate::ScopedEffectController<'scope>) -> Self {
        Self {
            parent_invocation: None,
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::borrowed(
                scoped_effect_controller,
            ),
            agent_frame_id: None,
            turn_cancellation: None,
        }
    }

    /// Sets the parent invocation carried by a `ProcessOpScope` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_parent_invocation(
        mut self,
        parent_invocation: Option<crate::RuntimeInvocation>,
    ) -> Self {
        self.parent_invocation = parent_invocation;
        self
    }

    /// Sets the agent frame id carried by a `ProcessOpScope` for store and durable-substrate
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_agent_frame_id(mut self, agent_frame_id: Option<crate::AgentFrameId>) -> Self {
        self.agent_frame_id = agent_frame_id;
        self
    }

    /// Attaches the turn cancellation this operation observes, taken from the
    /// complete turn-cancel trio so an operation that must not observe the
    /// turn gate cannot be handed a token-and-scope pair anyway.
    pub(crate) fn with_turn_cancellation(mut self, wait: &crate::runtime::TurnCancelWait) -> Self {
        self.turn_cancellation = wait.process_turn_cancellation();
        self
    }

    /// Exposes agent frame id to store and durable-substrate implementors while persisting and
    /// coordinating durable process execution. Returns `None` when no agent frame id is present.
    pub fn agent_frame_id(&self) -> Option<&str> {
        self.agent_frame_id.as_deref()
    }

    pub(crate) fn controller(&self) -> &dyn crate::RuntimeEffectController {
        self.effect_controller.controller()
    }
}
