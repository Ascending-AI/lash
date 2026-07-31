use super::*;
use std::sync::Arc;

#[async_trait::async_trait]
impl crate::runtime::effect::ProcessRunner for RuntimeSessionServices {
    async fn run_process(
        &self,
        registration: crate::ProcessRegistration,
        execution_context: crate::ProcessExecutionContext,
        registry: Arc<dyn crate::ProcessRegistry>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: tokio_util::sync::CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        let input = Arc::clone(&registration.input);
        // Hybrid process model by design:
        // - ToolCall, SessionTurn, and External are kernel primitives because
        //   core owns their orchestration contracts directly.
        // - Engine rows are deployment runtimes looked up from the registry.
        // This split keeps core process coordination explicit without pulling
        // language-specific runtimes into the kernel.
        match input.as_ref() {
            crate::ProcessInput::ToolCall { call } => {
                Ok(crate::ProcessRunOutcome::Terminal(Box::new(
                    self.run_process_tool_call(ProcessToolCallRun {
                        registration,
                        registry: Arc::clone(&registry),
                        call: call.clone(),
                        parent_invocation: execution_context.causal_invocation,
                        execution_write_authority: execution_context
                            .execution_write_authority
                            .expect("process worker installs execution write authority"),
                        scoped_effect_controller,
                        cancellation,
                    })
                    .await,
                )))
            }
            crate::ProcessInput::SessionTurn {
                create_request,
                turn_input,
                ..
            } => Ok(crate::ProcessRunOutcome::Terminal(Box::new(
                Box::pin(self.run_process_session_turn(
                    registration,
                    *create_request.clone(),
                    *turn_input.clone(),
                    scoped_effect_controller,
                    cancellation,
                ))
                .await,
            ))),
            crate::ProcessInput::Engine { kind, payload } => {
                let engine = match self.current.host.core.process_engines.require(kind) {
                    Ok(engine) => engine,
                    Err(err) => return Err(crate::ProcessInfraError::new(err)),
                };
                let engine_context = self.process_engine_run_context(
                    registration,
                    execution_context,
                    registry,
                    scoped_effect_controller,
                    cancellation,
                    handover,
                );
                engine.run(engine_context, payload.clone()).await
            }
            // Externally-owned rows are never executed by lash (ADR 0019): the
            // worker's run path rejects the disposition before dispatch, so this
            // is defensively unreachable. Never fabricate a success outcome for
            // work lash did not observe completing — surface a loud failure.
            crate::ProcessInput::External { .. } => {
                Err(crate::ProcessInfraError::new(crate::PluginError::Session(
                    "externally-owned process must not be executed by lash".to_string(),
                )))
            }
        }
    }
}

impl RuntimeSessionServices {
    fn process_engine_run_context<'run>(
        &self,
        registration: crate::ProcessRegistration,
        execution_context: crate::ProcessExecutionContext,
        registry: Arc<dyn crate::ProcessRegistry>,
        scoped_effect_controller: crate::ScopedEffectController<'run>,
        cancellation: tokio_util::sync::CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> crate::ProcessEngineRunContext<'run> {
        let session_id = self.current.session_id.clone();
        let plugins = Arc::clone(&self.current.plugins);
        let store = self.current.store.clone();
        let session_store_factory = self.current.host.session_store_factory.clone();
        let queued_work_driver = self.current.host.queued_work_driver.clone();
        let process_registry_available = self.current.host.process_registry.is_some();
        let process_awaiter = self
            .current
            .host
            .process_work_driver
            .as_ref()
            .map(crate::ProcessWorkDriver::awaiter)
            .unwrap_or_else(|| crate::ProcessAwaiter::polling(Arc::clone(&registry)));
        let services = self.clone();
        let registration_for_runtime = registration.clone();
        let execution_context_for_runtime = execution_context.clone();
        let execution_write_authority = execution_context
            .execution_write_authority
            .clone()
            .expect("process worker installs execution write authority");
        let registry_for_runtime = Arc::clone(&registry);
        let process_awaiter_for_runtime = process_awaiter.clone();
        let cancellation_for_runtime = cancellation.clone();
        let controller_for_context = scoped_effect_controller.clone();
        let builder = Box::new(move |tool_catalog: Arc<crate::ToolCatalog>| {
            let run_context = ProcessRunContext::builder(&services)
                .tool_catalog(tool_catalog)
                .scoped_effect_controller(scoped_effect_controller)
                .causal_invocation(execution_context_for_runtime.causal_invocation.clone())
                .build()?;
            let mut context = crate::RuntimeExecutionContext::new(
                services.current.session_id.clone(),
                run_context.dispatch(),
                Arc::clone(&services.current.host.core.durability.process_env_store),
                Arc::clone(&services.current.host.core.durability.attachment_store),
                Arc::new(crate::ChronologicalProjection::default()),
                None,
                crate::TurnContext::default(),
            )
            .with_execution_env_spec(current_execution_env_spec(&services.current))
            .with_turn_phase_probe(services.current.turn_phase_probe.clone())
            .with_process_registration_context(&registration_for_runtime)
            .with_process_event_context(
                registration_for_runtime.id.clone(),
                execution_write_authority.clone(),
                Arc::clone(&registry_for_runtime),
                process_awaiter_for_runtime.clone(),
                services.current.store.clone(),
                services.current.host.session_store_factory.clone(),
                services.current.host.queued_work_driver.clone(),
                Arc::clone(&services.current.host.core.clock),
                services.current.host.core.control.wake_turn_policy.clone(),
            )
            .with_cancellation_token(cancellation_for_runtime.clone())
            .without_turn_cancel_observation()
            .with_process_work_driver(services.current.host.process_work_driver.clone());
            if let Some(invocation) = execution_context_for_runtime.causal_invocation.clone() {
                context = context.with_parent_invocation(invocation);
            }
            let guard = crate::ProcessEngineRunGuard::new(move || {
                Box::pin(async move {
                    run_context.shutdown().await;
                })
            });
            Ok(crate::ProcessEngineRuntimeContext::new(context, guard))
        });
        crate::ProcessEngineRunContext::new(
            registration,
            execution_context,
            registry,
            process_awaiter,
            session_id,
            plugins,
            store,
            session_store_factory,
            queued_work_driver,
            Arc::clone(&self.current.host.core.clock),
            self.current.host.core.control.wake_turn_policy.clone(),
            process_registry_available,
            cancellation,
            self.current.turn_phase_probe.clone(),
            controller_for_context,
            handover,
            builder,
        )
    }
}

fn current_execution_env_spec(
    current: &CurrentSessionCapability,
) -> crate::ProcessExecutionEnvSpec {
    let state = current.snapshot.to_runtime_state();
    state.process_execution_env_spec(&current.policy)
}
