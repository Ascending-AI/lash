/// Whether `receiver` is the language-runtime receiver.
///
/// Only [`crate::lashlang_host_environment_from_tool_catalog`] registers
/// [`lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE`], so the type alone identifies
/// the receiver. Its *alias* does not: a front end emits one alias and linking
/// a module call rewrites the receiver to the catalog's resolved module ref,
/// whose alias is the module-path key. Both forms reach a host, so neither
/// alias may gate this dispatch (FIG-3079).
///
/// This is invoked only while resolving a VM `ResourceOperation` ability. That
/// suspension is the journal boundary: the sampled value is committed as the
/// ability outcome and replay never samples the clock or RNG again.
pub fn is_typescript_runtime_receiver(receiver: &lashlang::Value) -> bool {
    matches!(
        receiver,
        lashlang::Value::Resource(handle) if handle.resource_type == lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE
    )
}

/// The host operation a TypeScript runtime call's replay key names, and the
/// one its durable effect-summary record carries.
pub(crate) const TYPESCRIPT_RUNTIME_HOST_OPERATION: &str = "typescript.runtime";

pub async fn journaled_typescript_runtime_value(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    effect_id: String,
    receiver: &lashlang::Value,
    operation: &str,
    args: &[lashlang::Value],
) -> Option<Result<lashlang::Value, lashlang::ExecutionHostError>> {
    let mut journaled = false;
    journaled_typescript_runtime_value_recording(
        ctx,
        effect_id,
        receiver,
        operation,
        args,
        &mut journaled,
    )
    .await
}

/// As [`journaled_typescript_runtime_value`], also reporting through
/// `journaled` whether the effect produced a journaled value — the outcome a
/// process incorporates into its effect summary.
pub(crate) async fn journaled_typescript_runtime_value_recording(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    effect_id: String,
    receiver: &lashlang::Value,
    operation: &str,
    args: &[lashlang::Value],
    journaled: &mut bool,
) -> Option<Result<lashlang::Value, lashlang::ExecutionHostError>> {
    let lashlang::Value::Resource(handle) = receiver else {
        return None;
    };
    if handle.resource_type != lashlang::LANGUAGE_RUNTIME_RESOURCE_TYPE {
        return None;
    }
    if !args.is_empty() {
        return Some(Err(lashlang::ExecutionHostError::new(format!(
            "TypeScript runtime `{operation}` expects no arguments"
        ))));
    }
    if ![
        lashlang::LANGUAGE_RUNTIME_NOW_OPERATION,
        lashlang::LANGUAGE_RUNTIME_RANDOM_OPERATION,
    ]
    .contains(&operation)
    {
        return Some(Err(lashlang::ExecutionHostError::new(format!(
            "unknown TypeScript runtime operation `{operation}`"
        ))));
    }
    let value = ctx
        .journaled_language_runtime_value(effect_id, operation.to_string())
        .await;
    *journaled = value.is_ok();
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
