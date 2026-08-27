use super::*;

impl DurableProcessWorker {
    pub(super) async fn drive_pending_parent_end_actions(&self) -> Result<(), PluginError> {
        let limit = std::num::NonZeroUsize::new(256).expect("parent-end page bound is non-zero");
        loop {
            let plans = self
                .config
                .process_registry()
                .list_pending_parent_end_plans(limit)
                .await?;
            if plans.is_empty() {
                return Ok(());
            }
            for plan in plans {
                self.execute_parent_end_plan(plan).await?;
            }
        }
    }

    async fn drive_one_parent_end_plan(&self, process_id: &str) -> Result<(), PluginError> {
        if let Some(plan) = self
            .config
            .process_registry()
            .get_pending_parent_end_plan(process_id)
            .await?
        {
            self.execute_parent_end_plan(plan).await?;
        }
        Ok(())
    }

    async fn execute_parent_end_plan(
        &self,
        plan: crate::ProcessParentEndPlan,
    ) -> Result<(), PluginError> {
        let scoped_effect_controller = self
            .config
            .runtime_host
            .control
            .effect_host
            .scoped_static(crate::ExecutionScope::process(plan.process_id.clone()))
            .map_err(|error| PluginError::Session(error.to_string()))?
            .ok_or_else(|| {
                PluginError::Session(
                    "process worker effect host must provide a static parent-end scope".to_string(),
                )
            })?;
        self.execute_parent_end_plan_with_scoped_effect_controller(plan, scoped_effect_controller)
            .await
    }

    pub async fn execute_parent_end_plan_with_scoped_effect_controller(
        &self,
        plan: crate::ProcessParentEndPlan,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
    ) -> Result<(), PluginError> {
        let record = self
            .config
            .process_registry()
            .get_process(&plan.process_id)
            .await?
            .ok_or_else(|| {
                PluginError::Session(format!(
                    "parent-end plan references unknown process `{}`",
                    plan.process_id
                ))
            })?;
        if !record.is_terminal() {
            return Err(PluginError::Session(format!(
                "parent-end plan for `{}` became visible before terminal completion",
                plan.process_id
            )));
        }
        let registration = registration_from_record(record);
        let mut runtime = Box::pin(self.runtime_for_registration(&registration)).await?;
        let originator_scope = if let crate::ProcessOriginator::Session { session_id, .. } =
            &registration.provenance.originator
        {
            Some(crate::SessionScope::new(session_id))
        } else {
            None
        };
        let wake_scope = registration
            .wake_session_id
            .as_ref()
            .map(crate::SessionScope::new);
        if let Some(probe) = wake_scope
            .as_ref()
            .or(originator_scope.as_ref())
            .and_then(|scope| self.config.turn_phase_probe_slot.get_for_scope(scope))
        {
            runtime.set_turn_phase_probe(probe.clone());
            let _phase = crate::runtime::RuntimeNamedPhase::begin(
                Some(probe),
                "process.parent_end.after_terminal",
            );
        }
        let manager = RuntimeSessionServices::new(&runtime, true, None, None).map_err(|error| {
            PluginError::Session(format!(
                "failed to rebuild runtime env for parent-end plan `{}`: {error}",
                plan.process_id
            ))
        })?;
        manager
            .finish_process_parent_end_actions(scoped_effect_controller, &plan.actions)
            .await?;
        self.config
            .process_registry()
            .complete_parent_end_plan(&plan.process_id)
            .await
    }

    pub(super) async fn finish_terminal_run(
        &self,
        lease: &ProcessLease,
        process_id: &str,
        output: Box<ProcessAwaitOutput>,
        actions: Vec<crate::ToolIntentParentEndAction>,
    ) -> super::recovery::ProcessRecoveryOutcome {
        let completion = self
            .complete_and_release_with_parent_end(lease, process_id, *output, actions)
            .await;
        let terminal_written = matches!(
            completion,
            RecoveryCompletionDisposition::Committed
                | RecoveryCompletionDisposition::AlreadyApplied(_)
        );
        let outcome = completion.into_outcome();
        if terminal_written && let Err(error) = self.drive_one_parent_end_plan(process_id).await {
            tracing::warn!(
                process_id = %process_id,
                error = %error,
                "durable parent-end plan remains pending after terminal completion",
            );
        }
        outcome
    }
}
