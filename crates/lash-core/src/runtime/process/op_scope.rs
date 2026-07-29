#[derive(Clone)]
pub struct ProcessOpScope<'scope> {
    pub(crate) parent_invocation: Option<crate::RuntimeInvocation>,
    pub(crate) effect_controller: crate::runtime::RuntimeEffectControllerHandle<'scope>,
    pub(crate) agent_frame_id: Option<crate::AgentFrameId>,
    pub(crate) cancellation: tokio_util::sync::CancellationToken,
    pub(crate) observe_turn_cancel: bool,
    pub(crate) turn_cancel_scope: Option<crate::ExecutionScope>,
}

impl<'scope> ProcessOpScope<'scope> {
    pub fn new(scoped_effect_controller: crate::ScopedEffectController<'scope>) -> Self {
        Self {
            parent_invocation: None,
            effect_controller: crate::runtime::RuntimeEffectControllerHandle::borrowed(
                scoped_effect_controller,
            ),
            agent_frame_id: None,
            cancellation: tokio_util::sync::CancellationToken::new(),
            observe_turn_cancel: false,
            turn_cancel_scope: None,
        }
    }

    pub fn with_parent_invocation(
        mut self,
        parent_invocation: Option<crate::RuntimeInvocation>,
    ) -> Self {
        self.parent_invocation = parent_invocation;
        self
    }

    pub fn with_agent_frame_id(mut self, agent_frame_id: Option<crate::AgentFrameId>) -> Self {
        self.agent_frame_id = agent_frame_id;
        self
    }

    pub(crate) fn with_turn_cancel_options(
        mut self,
        cancellation: tokio_util::sync::CancellationToken,
        observe_turn_cancel: bool,
        scope: crate::ExecutionScope,
    ) -> Self {
        self.cancellation = cancellation;
        self.observe_turn_cancel = observe_turn_cancel;
        self.turn_cancel_scope = Some(scope);
        self
    }

    pub fn agent_frame_id(&self) -> Option<&str> {
        self.agent_frame_id.as_deref()
    }

    pub(crate) fn controller(&self) -> &dyn crate::RuntimeEffectController {
        self.effect_controller.controller()
    }
}
