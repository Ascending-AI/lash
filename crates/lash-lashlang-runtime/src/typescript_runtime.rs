/// Resolves the TypeScript dialect's nondeterministic standard-library calls.
///
/// This is invoked only while resolving a VM `ResourceOperation` ability. That
/// suspension is the journal boundary: the sampled value is committed as the
/// ability outcome and replay never samples the clock or RNG again.
pub fn is_typescript_runtime_receiver(receiver: &lashlang::Value) -> bool {
    matches!(
        receiver,
        lashlang::Value::Resource(handle)
            if handle.resource_type == "typescript.Runtime" && handle.alias == "builtin"
    )
}

pub async fn journaled_typescript_runtime_value(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    effect_id: String,
    receiver: &lashlang::Value,
    operation: &str,
    args: &[lashlang::Value],
) -> Option<Result<lashlang::Value, lashlang::ExecutionHostError>> {
    let lashlang::Value::Resource(handle) = receiver else {
        return None;
    };
    if handle.resource_type != "typescript.Runtime" || handle.alias != "builtin" {
        return None;
    }
    if !args.is_empty() {
        return Some(Err(lashlang::ExecutionHostError::new(format!(
            "TypeScript runtime `{operation}` expects no arguments"
        ))));
    }
    if !matches!(operation, "now" | "random") {
        return Some(Err(lashlang::ExecutionHostError::new(format!(
            "unknown TypeScript runtime operation `{operation}`"
        ))));
    }
    Some(
        ctx.journaled_language_runtime_value(effect_id, operation.to_string())
            .await
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
