use super::*;
use std::sync::Arc;

impl RuntimeSessionServices {
    /// Run an admitted process under these process-owned services; the
    /// [`crate::ProcessRuntimeContext`] is the runner that calls it.
    #[expect(
        clippy::expect_used,
        reason = "the process worker installs the write authority"
    )]
    pub(in crate::runtime) async fn run_admitted_process(
        &self,
        admitted: crate::runtime::effect::AdmittedProcess,
        execution_context: crate::ProcessExecutionContext,
        // Engine rows reach their registry through the process wiring the run
        // context builds, so this impl reads it from `self` rather than from
        // the argument the trait passes.
        _registry: Arc<dyn crate::ProcessRegistry>,
        scoped_effect_controller: crate::ActorContext,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        let crate::runtime::effect::AdmittedProcess {
            registration,
            process_id,
            trace,
        } = admitted;
        let scoped_effect_controller = match trace {
            Some(scope) => scoped_effect_controller.with_trace_scope(scope),
            None => scoped_effect_controller,
        };
        // The controller arrived already admitted for the process's minted id
        // (ADR 0099 §1, ADR 0107), so every arm below — a session-turn row's
        // cells reading it through their `RuntimeExecutionContext`, an engine
        // row's run context — runs under that
        // process by construction.
        let input = Arc::clone(&registration.input);
        // Hybrid process model by design:
        // - SessionTurn and External are kernel primitives because
        //   core owns their process contracts directly.
        // - Engine rows are deployment runtimes looked up from the registry.
        // This split keeps core process coordination explicit without pulling
        // language-specific runtimes into the kernel.
        match input.as_ref() {
            crate::ProcessInput::SessionTurn {
                create_request,
                turn_input,
                result,
                ..
            } => {
                let execution_write_authority = execution_context
                    .execution_write_authority
                    .expect("process worker installs execution write authority");
                let output = Box::pin(self.run_process_session_turn(
                    process_id.clone(),
                    registration.lineage(&process_id),
                    *create_request.clone(),
                    *turn_input.clone(),
                    result.clone(),
                    execution_write_authority,
                    scoped_effect_controller,
                    cancellation,
                ))
                .await?;
                Ok(crate::ProcessRunOutcome::from(output))
            }
            // An engine row is driven by `advance` from its process actor's
            // activation (ADR 0132 §10); it is never run here.
            crate::ProcessInput::Engine { kind, .. } => Err(crate::ProcessInfraError::new(
                crate::PluginError::Invoke(format!(
                    "process `{process_id}` runs engine `{kind}`, which its process actor drives by advance"
                )),
            )),
        }
    }
}
