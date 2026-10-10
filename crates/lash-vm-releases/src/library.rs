//! The library functions lash ships, each registered by the crate that
//! implements it: what the standard embedding holds before a dialect adds
//! its helpers.
//!
//! This crate's build script includes this file, and so does
//! `lash-vm-worker`: the build defines the TypeScript helpers and the
//! release the tree builds against these functions once, when this crate is
//! built, and a worker registers the functions again at startup beside
//! their native implementations. The library of this crate holds data
//! alone, and does not compile this file.

use std::sync::Arc;

use lash_kernel_doc::FunctionRegistry;

/// How many compiled patterns the regular-expression extension keeps.
const CACHED_PATTERNS: usize = 256;

/// Registers the kernel library and the machine's own functions.
pub(crate) fn register_kernel(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_kernel_lib::register_numbers(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_text_json(registry).map_err(|error| error.to_string())?;
    lash_kernel_vm::register_machine_functions(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_collections(registry).map_err(|error| error.to_string())?;
    Ok(())
}

/// Registers lash's extensions: ECMAScript regular expressions and dates,
/// and WHATWG URLs.
pub(crate) fn register_extensions(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_ext_regex_ecma::register(
        registry,
        &Arc::new(lash_ext_regex_ecma::Engine::new(CACHED_PATTERNS)),
    )
    .map_err(|error| error.to_string())?;
    lash_ext_date_ecma::register(registry).map_err(|error| error.to_string())?;
    lash_ext_url_whatwg::register(registry).map_err(|error| error.to_string())?;
    Ok(())
}
