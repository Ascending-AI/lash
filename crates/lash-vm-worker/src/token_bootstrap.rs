//! The image-loading portion of worker startup, shared with the kernel
//! benchmark. Source definitions are parsed only by the release build, and
//! a definition is decoded only when a process first reads it (FIG-6843).
use lash_kernel_doc::{FunctionId, FunctionRegistry, ReleasedFunctions};

/// Registers the build's release image `image` in place and returns its
/// functions.
pub(crate) fn register(
    registry: &mut FunctionRegistry,
    image: &'static [u8],
) -> Result<Vec<FunctionId>, String> {
    let image = ReleasedFunctions::read(image).map_err(|error| error.to_string())?;
    registry
        .register_released(image)
        .map_err(|error| error.to_string())?;
    Ok(image.ids().collect())
}

/// The kernel library and the machine's own functions: their definitions
/// from the build's images, and their natives added by name.
pub(crate) fn kernel() -> Result<FunctionRegistry, String> {
    let mut registry = FunctionRegistry::new();
    let base = register(&mut registry, lash_vm_releases::KERNEL_BASE)?;
    lash_kernel_lib::implement_numbers(&mut registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::implement_text_json(&mut registry).map_err(|error| error.to_string())?;
    lash_kernel_vm::implement_machine_functions(&mut registry)
        .map_err(|error| error.to_string())?;
    implemented(&registry, &base)?;
    register(&mut registry, lash_vm_releases::KERNEL_COLLECTIONS)?;
    lash_kernel_lib::implement_collections(&mut registry).map_err(|error| error.to_string())?;
    Ok(registry)
}

/// Refuses a registry in which one of `functions` has only a native
/// implementation and none was added: what registering it one by one
/// refuses (`RegistryError::NativeMissing`).
pub(crate) fn implemented(
    registry: &FunctionRegistry,
    functions: &[FunctionId],
) -> Result<(), String> {
    for function in functions {
        if let Some(registered) = registry.get(function)
            && registered.states_native()
            && !registered.has_body()
            && registered.native().is_none()
        {
            return Err(format!(
                "`{}` has only a native implementation and none was supplied",
                registered.name()
            ));
        }
    }
    Ok(())
}
