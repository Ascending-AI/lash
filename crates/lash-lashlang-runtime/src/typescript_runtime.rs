/// The reserved resource type the TypeScript lowerer mints for `Date.now()`,
/// `new Date()` and `Math.random()`. Only
/// [`crate::lashlang_host_environment_from_tool_catalog`] ever registers it, so
/// the type alone identifies the receiver.
///
/// The receiver's *alias* does not: the lowerer emits `builtin`
/// (`lash_typescript::lower::stdlib::journaled_runtime_call`), and linking a
/// module call rewrites the receiver to the catalog's resolved module ref,
/// whose alias is the module-path key `__typescript_runtime`. Both forms reach
/// a host, so neither alias may gate this dispatch (FIG-3079).
pub use lash_typescript::TYPESCRIPT_RUNTIME_RESOURCE_TYPE;

/// This is invoked only while resolving a VM `ResourceOperation` ability. That
/// suspension is the journal boundary: the sampled value is committed as the
/// ability outcome and replay never samples the clock or RNG again.
pub fn is_typescript_runtime_receiver(receiver: &lashlang::Value) -> bool {
    matches!(
        receiver,
        lashlang::Value::Resource(handle) if handle.resource_type == TYPESCRIPT_RUNTIME_RESOURCE_TYPE
    )
}

pub async fn journaled_typescript_runtime_value(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    effect_id: String,
    receiver: &lashlang::Value,
    operation: &str,
    args: &[lashlang::Value],
) -> Option<Result<lashlang::Value, lashlang::ExecutionHostError>> {
    journaled_typescript_runtime_value_inner(ctx, effect_id, receiver, operation, args, None).await
}

pub(crate) async fn journaled_process_typescript_runtime_value(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    effect_id: String,
    receiver: &lashlang::Value,
    operation: &str,
    args: &[lashlang::Value],
    call_site: &lashlang::LashlangExecutionCallSite,
) -> Option<Result<lashlang::Value, lashlang::ExecutionHostError>> {
    journaled_typescript_runtime_value_inner(
        ctx,
        effect_id,
        receiver,
        operation,
        args,
        Some(call_site),
    )
    .await
}

async fn journaled_typescript_runtime_value_inner(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    effect_id: String,
    receiver: &lashlang::Value,
    operation: &str,
    args: &[lashlang::Value],
    call_site: Option<&lashlang::LashlangExecutionCallSite>,
) -> Option<Result<lashlang::Value, lashlang::ExecutionHostError>> {
    let lashlang::Value::Resource(handle) = receiver else {
        return None;
    };
    if handle.resource_type != TYPESCRIPT_RUNTIME_RESOURCE_TYPE {
        return None;
    }
    if !args.is_empty() {
        return Some(Err(lashlang::ExecutionHostError::new(format!(
            "TypeScript runtime `{operation}` expects no arguments"
        ))));
    }
    if ![
        lash_typescript::TYPESCRIPT_RUNTIME_NOW_OPERATION,
        lash_typescript::TYPESCRIPT_RUNTIME_RANDOM_OPERATION,
    ]
    .contains(&operation)
    {
        return Some(Err(lashlang::ExecutionHostError::new(format!(
            "unknown TypeScript runtime operation `{operation}`"
        ))));
    }
    let value = ctx
        .journaled_language_runtime_value(effect_id.clone(), operation.to_string())
        .await;
    if value.is_ok()
        && let Some(call_site) = call_site
        && let Err(error) = ctx
            .append_process_event(
                lash_core::ProcessEffectSummaryOccurrence::new(
                    call_site.site.node_id.clone(),
                    call_site.occurrence,
                    operation,
                    lash_core::ProcessEffectOutcomeClass::Success,
                    None,
                    effect_id,
                )
                .append_request(),
            )
            .await
    {
        return Some(Err(lashlang::ExecutionHostError::new(error.to_string())));
    }
    Some(
        value
            .map_err(|error| lashlang::ExecutionHostError::new(error.to_string()))
            .and_then(|value| {
                value.as_f64().map(lashlang::Value::Number).ok_or_else(|| {
                    lashlang::ExecutionHostError::new(format!(
                        "journaled TypeScript runtime `{operation}` returned a non-number"
                    ))
                })
            }),
    )
}
