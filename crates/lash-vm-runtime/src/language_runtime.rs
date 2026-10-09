/// Whether `receiver` is the language-runtime receiver.
///
/// Only [`crate::lash_vm_host_environment_from_tool_catalog`] registers
/// [`lash_vm::LANGUAGE_RUNTIME_RESOURCE_TYPE`], so the type alone identifies
/// the receiver. Its *alias* does not: a front end emits one alias and linking
/// a module call rewrites the receiver to the catalog's resolved module ref,
/// whose alias is the module-path key. Both forms reach a host, so neither
/// alias may gate this dispatch (FIG-3079).
///
/// This is invoked only while resolving a VM `ResourceOperation` ability. That
/// suspension is the journal boundary: the sampled value is committed as the
/// ability outcome and replay never samples the clock or RNG again.
pub fn is_language_runtime_receiver(receiver: &lash_vm::Value) -> bool {
    matches!(
        receiver,
        lash_vm::Value::Resource(handle) if handle.resource_type == lash_vm::LANGUAGE_RUNTIME_RESOURCE_TYPE
    )
}

/// The language runtime operation a call names, checked before anything
/// reaches the journal: `None` when `receiver` is not the runtime, a refusal
/// for arguments or an operation the runtime does not have.
pub fn language_runtime_operation<'op>(
    receiver: &lash_vm::Value,
    operation: &'op str,
    args: &[lash_vm::Value],
) -> Option<Result<&'op str, lash_vm::ExecutionHostError>> {
    let lash_vm::Value::Resource(handle) = receiver else {
        return None;
    };
    if handle.resource_type != lash_vm::LANGUAGE_RUNTIME_RESOURCE_TYPE {
        return None;
    }
    if !args.is_empty() {
        return Some(Err(lash_vm::ExecutionHostError::new(format!(
            "language runtime `{operation}` expects no arguments"
        ))));
    }
    if ![
        lash_vm::LANGUAGE_RUNTIME_NOW_OPERATION,
        lash_vm::LANGUAGE_RUNTIME_RANDOM_OPERATION,
    ]
    .contains(&operation)
    {
        return Some(Err(lash_vm::ExecutionHostError::new(format!(
            "unknown language runtime operation `{operation}`"
        ))));
    }
    Some(Ok(operation))
}

/// Journals one checked language runtime operation at `key` and answers
/// its value. The outer error is the journal's — a replay mismatch among
/// them, which a bridge stops the run on — and the inner one the value's.
pub async fn journaled_language_runtime_value(
    ctx: &lash_core::RuntimeExecutionContext<'_>,
    key: String,
    operation: &str,
) -> Result<
    Result<lash_vm::Value, lash_vm::ExecutionHostError>,
    lash_core::RuntimeEffectControllerError,
> {
    let value = ctx
        .journaled_language_runtime_value(key, operation.to_string())
        .await?;
    Ok(value.as_f64().map(lash_vm::Value::Number).ok_or_else(|| {
        lash_vm::ExecutionHostError::new(format!(
            "journaled language runtime `{operation}` returned a non-number"
        ))
    }))
}
