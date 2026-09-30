#[derive(Clone)]
pub struct ProcessOpScope<'scope> {
    pub parent_invocation: Option<crate::RuntimeInvocation>,
    pub effect_controller: crate::runtime::ScopedEffectController<'scope>,
    pub agent_frame_id: Option<crate::FrameNodeId>,
    pub turn_cancellation: Option<crate::ProcessTurnCancellation>,
    /// The lineage of the process this operation runs inside, when it runs
    /// inside one: what a start made here records above its starter.
    pub process_lineage: Option<crate::ProcessLineage>,
    /// The parked call a start made here registers its child under, when the
    /// start is a call's declared start (ADR 0116 §3.6).
    pub consumer_hold: Option<crate::ConsumerHold>,
}

impl<'scope> ProcessOpScope<'scope> {
    /// Constructs a `ProcessOpScope` for store and durable-substrate implementors while persisting
    /// and coordinating durable process execution.
    pub fn new(scoped_effect_controller: crate::ScopedEffectController<'scope>) -> Self {
        Self {
            parent_invocation: None,
            effect_controller: scoped_effect_controller,
            agent_frame_id: None,
            turn_cancellation: None,
            process_lineage: None,
            consumer_hold: None,
        }
    }

    /// Registers a start made under this operation with a parked call's hold.
    pub fn with_consumer_hold(mut self, consumer_hold: Option<crate::ConsumerHold>) -> Self {
        self.consumer_hold = consumer_hold;
        self
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
            self.effect_controller.admitted_scope(),
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
        crate::StartCx::materialize(self.effect_controller.admitted_scope(), Some(lineage))
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

#[cfg(test)]
mod controller_tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn operation_controller_clones_preserve_admission_and_journal_guard() {
        let admitted = crate::AdmittedScope::process(crate::process_id_for_test("operation"));
        let controller = crate::ScopedEffectController::shared(
            Arc::new(crate::testing::UnavailableEffectController),
            admitted.clone(),
        )
        .expect("valid process scope");
        let refusal = crate::RuntimeEffectControllerError::new(
            crate::RuntimeErrorCode::ExecutionScopeAdmissionRefused,
            "refused command write",
        );
        let guard = Arc::new(crate::CommandJournalGuard::refusing(
            crate::RefusedWriteRange {
                lower: "command/".into(),
                upper: "command/~".into(),
                refusal: refusal.clone(),
            },
        ));
        let operation = ProcessOpScope::new(controller);
        let guarded = operation
            .effect_controller
            .with_journal_guard(Arc::clone(&guard));
        let cloned = guarded.clone();
        let static_controller = cloned.to_static().expect("shared controller is static");
        for controller in [guarded.clone(), cloned.clone(), static_controller.clone()] {
            assert_eq!(controller.admitted_scope(), &admitted);
            assert_eq!(controller.admitted_process(), admitted.process_id());
            assert!(Arc::ptr_eq(
                &controller.journal_guard().expect("guard"),
                &guard
            ));
            assert!(controller.admit_journal_write_at(Some("outside")).is_ok());
        }
        let error = static_controller
            .admit_journal_write_at(Some("command/write"))
            .expect_err("guard refuses the command write");
        assert_eq!(error.code, refusal.code);
        assert_eq!(error.message, refusal.message);
        let tripped = guard.tripped().expect("guard recorded the refusal");
        assert_eq!(tripped.code, refusal.code);
        assert_eq!(tripped.message, refusal.message);
        assert!(guard.touched());
    }

    #[test]
    fn shared_operation_controller_clones_share_start_and_compaction_ordinals() {
        let operation = ProcessOpScope::new(
            crate::ScopedEffectController::shared(
                Arc::new(crate::testing::UnavailableEffectController),
                crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
            )
            .expect("valid runtime scope"),
        );
        let clone = operation.clone();
        let first = operation.effect_controller.clone();
        let second = clone.effect_controller.clone();
        assert_ne!(
            first.next_keyless_start_key(),
            second.next_keyless_start_key()
        );
        assert_eq!(first.next_compaction_ordinal(), 0);
        assert_eq!(second.next_compaction_ordinal(), 1);
        let static_controller = operation
            .effect_controller
            .to_static()
            .expect("shared is static");
        assert_eq!(static_controller.next_compaction_ordinal(), 2);
    }
}
