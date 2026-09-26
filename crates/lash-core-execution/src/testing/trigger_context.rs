use super::{TestExecutionContextBuilder, TestExecutionPorts, process_work_wiring_for_registry};
use std::sync::Arc;

/// Build an empty code-execution context whose trigger router delivers through
/// `trigger_store` to processes in `process_registry`.
pub fn code_execution_context_with_trigger_store<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    trigger_store: Arc<dyn crate::TriggerStore>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .trigger_router(Some(test_trigger_router(trigger_store, process_registry)))
        .build()
        .into_runtime()
}

/// [`code_execution_context_with_trigger_store`] under a stable parent
/// invocation.
pub fn code_execution_context_with_trigger_store_and_invocation<'run>(
    ports: impl Into<TestExecutionPorts<'run>>,
    trigger_store: Arc<dyn crate::TriggerStore>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .trigger_router(Some(test_trigger_router(trigger_store, process_registry)))
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

/// A trigger router over `trigger_store`, for fixtures building their own context.
pub fn test_trigger_router(
    trigger_store: Arc<dyn crate::TriggerStore>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
) -> crate::TriggerRouter {
    crate::TriggerRouter::new(
        trigger_store,
        process_work_wiring_for_registry(process_registry),
    )
}
