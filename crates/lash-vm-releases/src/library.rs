//! The library functions lash ships, each registered by the crate that
//! implements it: what an embedding holds before a dialect adds its
//! helpers. Every embedding holds the kernel library; the extensions are
//! the TypeScript dialect's (FIG-5990). Each dialect's hot operations have
//! partial natives in a dialect-free crate, added to the helpers once they
//! are registered (FIG-6440).
//!
//! This crate's build script includes this file, and so does
//! `lash-vm-worker`: the build defines each dialect's helpers and the
//! release the tree builds against these functions once, when this crate is
//! built, and a worker registers the functions again at startup beside
//! their native implementations. The library of this crate holds data
//! alone, and does not compile this file.

use lash_kernel_doc::FunctionRegistry;

/// How many compiled patterns the regular-expression extension keeps.
#[cfg(feature = "dialect-typescript")]
const CACHED_PATTERNS: usize = 256;

/// Registers the kernel's native base and the machine's own functions.
/// Collection definitions are supplied separately by the release build.
/// The build writes these definitions into its kernel image; a worker
/// registers that image and adds the natives (FIG-6843).
#[allow(
    dead_code,
    reason = "the build registers the base from source; a worker only in its laws"
)]
pub(crate) fn register_kernel_base(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_kernel_lib::register_numbers(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_text_json(registry).map_err(|error| error.to_string())?;
    lash_kernel_vm::register_machine_functions(registry).map_err(|error| error.to_string())?;
    Ok(())
}

/// Registers the regular-expression extension, which the build writes
/// into the TypeScript extensions' image beside the date and URL
/// definitions.
#[cfg(feature = "dialect-typescript")]
#[allow(
    dead_code,
    reason = "the build registers the extension from source; a worker only in its laws"
)]
pub(crate) fn register_extensions(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_ext_regex_ecma::register(
        registry,
        &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(CACHED_PATTERNS)),
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Adds the regular-expression natives to the definitions the
/// TypeScript extensions' image holds.
#[cfg(feature = "dialect-typescript")]
#[allow(
    dead_code,
    reason = "a worker adds the natives; the build registers them"
)]
pub(crate) fn implement_extensions(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_ext_regex_ecma::implement(
        registry,
        &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(CACHED_PATTERNS)),
    )
    .map(|_| ())
    .map_err(|error| error.to_string())
}

/// Adds the natives of the TypeScript dialect's operations to its
/// registered `helpers`.
#[cfg(feature = "dialect-typescript")]
pub(crate) fn implement_typescript(
    registry: &mut FunctionRegistry,
    helpers: &[lash_kernel_doc::FunctionId],
) -> Result<(), String> {
    lash_ext_ts_core::implement(registry, helpers.iter().copied())
        .map_err(|error| error.to_string())
}

/// Adds the natives of the Python dialect's operations to its registered
/// `helpers`.
#[cfg(feature = "dialect-python")]
pub(crate) fn implement_python(
    registry: &mut FunctionRegistry,
    helpers: &[lash_kernel_doc::FunctionId],
) -> Result<(), String> {
    lash_ext_py_core::implement(registry, helpers.iter().copied())
        .map_err(|error| error.to_string())
}
