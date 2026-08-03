use super::*;
use crate::facade_support::ScopedEffectControllerFacadeOps;
use std::sync::Arc;

impl RuntimeSessionServices {
    pub(in crate::runtime::session_manager::process_runners) async fn run_process_tool_call(
        &self,
        run: ProcessToolCallRun<'_>,
    ) -> crate::ProcessAwaitOutput {
        let result = self.execute_process_tool_call(run).await;
        match result {
            Ok(output) => crate::ProcessAwaitOutput::from_tool_output(output),
            Err(err) => crate::ProcessAwaitOutput::from_tool_output(
                crate::ToolCallOutput::failure(crate::ToolFailure::runtime(
                    crate::ToolFailureClass::Internal,
                    "process_tool_failed",
                    err.to_string(),
                )),
            ),
        }
    }

    async fn execute_process_tool_call(
        &self,
        run: ProcessToolCallRun<'_>,
    ) -> Result<crate::ToolCallOutput, crate::PluginError> {
        let ProcessToolCallRun {
            registration,
            registry,
            call,
            parent_invocation,
            execution_write_authority,
            scoped_effect_controller,
            cancellation,
        } = run;
        let awaiter = self
            .current
            .host
            .process_work_driver
            .as_ref()
            .map(crate::ProcessWorkDriver::awaiter)
            .unwrap_or_else(|| crate::ProcessAwaiter::polling(Arc::clone(&registry)));
        let await_parent_invocation = parent_invocation.clone();
        let await_cancellation = cancellation.clone();
        let run_context = ProcessRunContext::builder(self)
            .tool_catalog(
                self.current
                    .plugins
                    .resolved_tool_catalog(&self.current.session_id)?,
            )
            .scoped_effect_controller(scoped_effect_controller)
            .causal_invocation(parent_invocation.clone())
            .dispatch_parent_invocation(parent_invocation)
            .build()?;
        let dispatch = run_context.dispatch();
        let tool_context = crate::ToolContext::from_dispatch(Arc::clone(&dispatch))
            .prepared_call(&call)
            .async_process(registration.id.clone(), cancellation)
            .process_events(
                registration.id.clone(),
                execution_write_authority,
                registry,
                awaiter,
                self.current.store.clone(),
                self.current.host.session_store_factory.clone(),
                self.current.host.queued_work_driver.clone(),
                Arc::clone(&self.current.host.core.clock),
                self.current.host.core.control.wake_turn_policy.clone(),
            )
            .build();
        let retry_policy =
            crate::tool_dispatch::resolve_callable_manifest_by_id(dispatch.as_ref(), &call.tool_id)
                .map(|manifest| manifest.retry_policy)
                .unwrap_or(crate::ToolRetryPolicy::Never);
        let coordinated = crate::tool_dispatch::coordinate_tool_invocation(
            dispatch.as_ref(),
            call,
            None,
            retry_policy,
            crate::tool_dispatch::ToolAttemptEffectIdentity::Process {
                parent: await_parent_invocation.clone(),
                process_id: registration.id.clone(),
            },
            Some(await_cancellation.clone()),
            || {
                crate::RuntimeEffectLocalExecutor::prepared_tool_attempt(
                    Arc::clone(&dispatch),
                    tool_context.clone(),
                )
            },
        )
        .await;
        drop(tool_context);
        let launch = coordinated.launch;
        let output = match launch {
            crate::tool_dispatch::ToolCallLaunch::Done(outcome) => outcome.record.output,
            crate::tool_dispatch::ToolCallLaunch::Pending(pending) => {
                let fallback;
                let parent = if let Some(parent) = await_parent_invocation.as_ref() {
                    parent
                } else {
                    fallback = crate::RuntimeInvocation::effect(
                        crate::RuntimeScope::new(&self.current.session_id),
                        format!(
                            "process:{}:tool:{}:await",
                            registration.id, pending.tool_name
                        ),
                        crate::RuntimeEffectKind::AwaitEvent,
                        format!(
                            "process:{}:tool:{}:await",
                            registration.id, pending.tool_name
                        ),
                    );
                    &fallback
                };
                let invocation = crate::runtime::causal::child_effect_invocation(
                    parent,
                    format!(
                        "process:{}:tool:{}:await",
                        registration.id, pending.tool_name
                    ),
                    crate::RuntimeEffectKind::AwaitEvent,
                    format!(
                        "process:{}:tool:{}:await",
                        registration.id, pending.tool_name
                    ),
                );
                let outcome = Box::pin(
                    dispatch.effect_controller.controller().execute_effect(
                        crate::RuntimeEffectEnvelope::new(
                            invocation,
                            crate::RuntimeEffectCommand::AwaitEvent { key: pending.key },
                        ),
                        crate::RuntimeEffectLocalExecutor::await_event_with_clock(
                            await_cancellation.clone(),
                            pending
                                .pending
                                .deadline
                                .map(|duration| dispatch.clock.now() + duration),
                            std::sync::Arc::clone(&dispatch.clock),
                        )
                        .with_turn_cancel_scope(
                            dispatch
                                .effect_controller
                                .scoped()
                                .execution_scope()
                                .clone(),
                        ),
                    ),
                )
                .await
                .map_err(|err| crate::PluginError::Session(err.to_string()))?;
                let resolution = outcome
                    .into_await_event()
                    .map_err(|err| crate::PluginError::Session(err.to_string()))?;
                crate::tool_result::tool_output_from_completion_resolution(resolution)
            }
        };
        drop(dispatch);
        run_context.shutdown().await;
        Ok(output)
    }
}
