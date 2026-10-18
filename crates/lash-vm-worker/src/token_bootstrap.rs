//! The token-loading portion of worker startup, shared with the kernel
//! benchmark. Source definitions are parsed only by the release build.
use lash_kernel_doc::{FunctionId, FunctionRegistry, ValidatedFunctions};

pub(crate) fn register(
    registry: &mut FunctionRegistry,
    tokens: &[u8],
) -> Result<Vec<FunctionId>, String> {
    let definitions = ValidatedFunctions::from_tokens(tokens).map_err(|error| error.to_string())?;
    let functions = definitions.iter().map(|(function, _)| *function).collect();
    registry
        .register_validated(definitions)
        .map_err(|error| error.to_string())?;
    Ok(functions)
}

pub(crate) fn kernel(
    base: impl FnOnce(&mut FunctionRegistry) -> Result<(), String>,
) -> Result<FunctionRegistry, String> {
    let mut registry = FunctionRegistry::new();
    base(&mut registry)?;
    register(&mut registry, lash_vm_releases::KERNEL_COLLECTIONS)?;
    lash_kernel_lib::implement_collections(&mut registry).map_err(|error| error.to_string())?;
    Ok(registry)
}
