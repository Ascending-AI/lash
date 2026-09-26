#[derive(Clone)]
pub struct ProcessOpScope<'scope> {
    pub parent_invocation: Option<crate::RuntimeInvocation>,
    pub effect_controller: crate::runtime::RuntimeEffectControllerHandle<'scope>,
    pub agent_frame_id: Option<crate::FrameNodeId>,
    pub turn_cancellation: Option<crate::ProcessTurnCancellation>,
    /// The lineage of the process this operation runs inside, when it runs
    /// inside one: what a start made here records above its starter.
    pub process_lineage: Option<crate::ProcessLineage>,
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
            process_lineage: None,
        }
    }

    /// Sets the lineage of the process this operation runs inside.
    pub fn with_process_lineage(mut self, lineage: Option<crate::ProcessLineage>) -> Self {
        self.process_lineage = lineage;
        self
    }

    /// The start context a runtime start made under this operation records
    /// (FIG-3607 R2): the admitted scope and the enclosing process's lineage.
    /// `Ok(None)` under an administrative scope, which names no opener: the
    /// start registers as a root and may only be `Detached`. A process scope
    /// run without its lineage answers `MissingLineage` rather than a root, so
    /// a start never silently loses the ancestry it ran under.
    pub fn start_cx(&self) -> Result<Option<crate::StartCx>, crate::StartCxError> {
        match crate::StartCx::materialize(
            self.effect_controller.scoped().admitted_scope(),
            self.process_lineage.as_ref(),
        ) {
            Ok(cx) => Ok(Some(cx)),
            Err(crate::StartCxError::NotAnOpener(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// The start context under this operation's admitted scope with the
    /// enclosing process's `lineage` read back from its row, for a context
    /// that runs inside the process without carrying its lineage.
    pub fn start_cx_under(
        &self,
        lineage: &crate::ProcessLineage,
    ) -> Result<crate::StartCx, crate::StartCxError> {
        crate::StartCx::materialize(
            self.effect_controller.scoped().admitted_scope(),
            Some(lineage),
        )
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
    pub fn with_agent_frame_id(mut self, agent_frame_id: Option<crate::FrameNodeId>) -> Self {
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
    /// coordinating durable process execution.
    pub fn agent_frame_id(&self) -> Option<&crate::FrameNodeId> {
        self.agent_frame_id.as_ref()
    }

    pub fn controller(&self) -> &dyn crate::RuntimeEffectController {
        self.effect_controller.controller()
    }
}
