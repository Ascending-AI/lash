use super::*;
use lash_core_execution::core_internal::RuntimeExecutionContextRuntimeOps as _;
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
        registry: Arc<dyn crate::ProcessRegistry>,
        scoped_effect_controller: crate::ScopedEffectController<'_>,
        cancellation: tokio_util::sync::CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessRunOutcome, crate::ProcessInfraError> {
        let crate::runtime::effect::AdmittedProcess {
            registration,
            process_id,
        } = admitted;
        let retained_scope = registry
            .get_process(&process_id)
            .await
            .map_err(crate::ProcessInfraError::new)?
            .and_then(|record| record.trace);
        let scoped_effect_controller = match retained_scope {
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
            crate::ProcessInput::Engine { kind, payload } => {
                let engine = match self.current.host.core.process_engines.require(kind) {
                    Ok(engine) => engine,
                    Err(err) => return Err(crate::ProcessInfraError::new(err)),
                };
                let engine_context = self.process_engine_run_context(
                    registration,
                    process_id,
                    execution_context,
                    scoped_effect_controller,
                    cancellation,
                    handover,
                )?;
                let standing = engine_context.trace_standing();
                let outcome = engine.run(engine_context, payload.clone()).await;
                if outcome.is_ok() {
                    standing.conclude();
                }
                outcome
            }
            // Externally-owned rows are never executed by lash (ADR 0110): the
            // worker's run path rejects them before dispatch, so this
            // is defensively unreachable. Never fabricate a success outcome for
            // work lash did not observe completing — surface a loud failure.
            crate::ProcessInput::External { .. } => Err(crate::ProcessInfraError::new(
                crate::PluginError::attempt_fault(
                    "externally-owned process must not be executed by lash".to_string(),
                ),
            )),
        }
    }
}

impl RuntimeSessionServices {
    #[expect(
        clippy::expect_used,
        reason = "the process worker installs its process wiring and write authority"
    )]
    fn process_engine_run_context<'run>(
        &self,
        registration: crate::ProcessRegistration,
        process_id: crate::ProcessId,
        execution_context: crate::ProcessExecutionContext,
        scoped_effect_controller: crate::ScopedEffectController<'run>,
        cancellation: tokio_util::sync::CancellationToken,
        handover: Option<crate::SegmentHandover>,
    ) -> Result<crate::ProcessEngineRunContext<'run>, crate::PluginError> {
        let plugins = Arc::clone(&self.current.plugins);
        let store = self.current.session_runtime_store();
        let session_store_factory = Some(self.current.host.core.session_store_factory());
        let queued_work = Arc::clone(self.current.host.queued_work());
        let process_registry_available = self.current.host.process_registry().is_some();
        let process_work = self
            .current
            .host
            .work
            .process_wiring()
            .cloned()
            .expect("process runner requires process-work wiring");
        let services = self.clone();
        let registration_for_runtime = registration.clone();
        let process_id_for_runtime = process_id.clone();
        let execution_context_for_runtime = execution_context.clone();
        let execution_write_authority = execution_context
            .execution_write_authority
            .clone()
            .expect("process worker installs execution write authority");
        let process_work_for_runtime = process_work.clone();
        let cancellation_for_runtime = cancellation.clone();
        let controller_for_context = scoped_effect_controller.clone();
        let retained_scope = scoped_effect_controller.trace_scope().cloned();
        let tool_surface = plugins.pin_resolved_tool_surface()?;
        let tool_catalog = Arc::clone(&tool_surface.catalog);
        let builder = Box::new(move |requested_catalog: Arc<crate::ToolCatalog>| {
            if !Arc::ptr_eq(&requested_catalog, &tool_surface.catalog) {
                return Err(crate::PluginError::Session(
                    "process engine runtime context requires its captured tool catalog".into(),
                ));
            }
            let run_context = ProcessRunContext::builder(&services)
                .tool_surface(tool_surface)
                .process_lineage(registration_for_runtime.lineage(&process_id_for_runtime))
                .process_originator(registration_for_runtime.provenance.originator.clone())
                .scoped_effect_controller(scoped_effect_controller)
                .causal_invocation(execution_context_for_runtime.causal_invocation.clone())
                .cancellation(cancellation_for_runtime.clone())
                .build()?;
            let dispatch = run_context.dispatch();
            let event_context = crate::RuntimeExecutionProcessEventContext {
                execution_write_authority: execution_write_authority.clone(),
                process_work: process_work_for_runtime.clone(),
                store: services.current.session_runtime_store(),
                session_store_factory: Some(services.current.host.core.session_store_factory()),
                queued_work: Arc::clone(services.current.host.queued_work()),
                process_wake_delivery_policy: services
                    .current
                    .host
                    .core
                    .control
                    .process_wake_delivery_policy,
                clock: Arc::clone(&services.current.host.core.clock),
            };
            let mut context = crate::RuntimeExecutionContext::new(
                Arc::clone(&dispatch),
                Arc::clone(&services.current.host.core.durability.process_env_store),
                Arc::clone(&services.current.host.core.durability.attachment_store),
                Arc::new(crate::ChronologicalProjection::default()),
                crate::TurnContext::default(),
                services.current.execution_env_spec()?,
            )
            // The process's durable stamps (effect occurrences, its terminal
            // prelude) follow the `F` its store recorded, never this build's
            // own epoch: N+1 writes N's formats until finalize (FIG-3805).
            .with_tool_material_store(services.current.host.core.backend().tool_material_store())
            .with_fleet_format(services.current.fleet_format())
            .with_turn_phase_probe(services.current.turn_phase_probe.clone())
            .with_process_execution(
                process_id_for_runtime.clone(),
                &registration_for_runtime,
                event_context,
            )
            .with_lent_process_stop(cancellation_for_runtime.clone())
            .without_turn_cancel_observation()
            .with_process_work(services.current.host.work.process_wiring().cloned())
            .with_opener_state(crate::session::OpenerState::default())
            .with_unrecorded_session_sources(services.current.host.core.control.open_sources);
            // What runs in the process observes through the runtime's shared
            // handle, under the process's scope.
            let tracing = &services.current.host.core.tracing;
            if tracing.is_observed() || tracing.emitter().has_product_observers() {
                context = context.with_tracing(Some(crate::RuntimeExecutionTracing::new(
                    tracing.clone(),
                    retained_scope.clone(),
                    lash_trace::TraceContext::default(),
                )));
            }
            if let Some(invocation) = execution_context_for_runtime.causal_invocation.clone() {
                context = context.with_parent_invocation(invocation);
            }
            let context_for_shutdown = context.clone();
            let guard = crate::ProcessEngineRunGuard::new(move |parent_ended| {
                Box::pin(async move {
                    debug_assert!(
                        !parent_ended,
                        "process teardown belongs after durable terminal completion"
                    );
                    let nested_effect_error = context_for_shutdown.take_nested_effect_error();
                    drop(context_for_shutdown);
                    drop(dispatch);
                    run_context.shutdown().await;
                    if let Some(error) = nested_effect_error {
                        return Err(crate::PluginError::RuntimeEffectController(error));
                    }
                    Ok(())
                })
            });
            Ok(crate::ProcessEngineRuntimeContext::new(context, guard))
        });
        Ok(crate::ProcessEngineRunContext::new(
            registration,
            process_id,
            execution_context,
            process_work,
            plugins,
            tool_catalog,
            store,
            session_store_factory,
            queued_work,
            self.current.host.core.control.process_wake_delivery_policy,
            Arc::clone(&self.current.host.core.clock),
            process_registry_available,
            cancellation,
            self.current.turn_phase_probe.clone(),
            controller_for_context,
            handover,
            builder,
        )
        .with_trace_runtime(self.current.host.core.tracing.clone()))
    }
}
