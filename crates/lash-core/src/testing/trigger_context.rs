use super::{TestExecutionContextBuilder, process_work_wiring_for_registry};
use std::sync::Arc;

pub fn code_execution_context_with_trigger_store(
    trigger_store: Arc<dyn crate::TriggerStore>,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .trigger_router(Some(test_trigger_router(trigger_store)))
        .build()
        .into_runtime()
}

/// Build a trigger-capable code-execution context carrying the stable parent
/// invocation used by deferred link resolution.
pub fn code_execution_context_with_trigger_store_and_invocation(
    trigger_store: Arc<dyn crate::TriggerStore>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .trigger_router(Some(test_trigger_router(trigger_store)))
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

/// Build a code-execution context whose trigger operations pass through the
/// supplied effect controller before reaching `trigger_store`.
pub fn code_execution_context_with_trigger_store_and_effect_controller(
    trigger_store: Arc<dyn crate::TriggerStore>,
    effect_controller: Arc<dyn crate::RuntimeEffectController>,
) -> crate::RuntimeExecutionContext<'static> {
    TestExecutionContextBuilder::new()
        .trigger_router(Some(test_trigger_router(trigger_store)))
        .shared_effect_controller(effect_controller)
        .build()
        .into_runtime()
}

fn test_trigger_router(trigger_store: Arc<dyn crate::TriggerStore>) -> crate::TriggerRouter {
    let registry: Arc<dyn crate::ProcessRegistry> =
        Arc::new(crate::TestLocalProcessRegistry::default());
    crate::TriggerRouter::new(trigger_store, process_work_wiring_for_registry(registry))
}
