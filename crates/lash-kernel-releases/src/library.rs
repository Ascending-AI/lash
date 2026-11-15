//! lash's own packages (FIG-6917): each one's definitions, as the crate that
//! implements it registers them, and its natives, by the package's path.
//! Every embedding holds `kernel`; `regex.ecma`, `date.ecma`, `url.whatwg`
//! and `typescript` are the TypeScript dialect's (FIG-5990), and `python`
//! the Python dialect's. Each dialect's hot operations have partial natives
//! in a dialect-free crate (FIG-6440).
//!
//! This crate's build script uses these definitions to produce release data.
//! Runtime native construction belongs to `lash-kernel-engine`.

use lash_kernel_doc::FunctionRegistry;
use lash_kernel_value::names::FunctionId;

/// The release label of every lash package this tree builds.
#[allow(
    dead_code,
    reason = "the build labels the packages; a worker reads none"
)]
pub(crate) const RELEASE: &str = "1.0";

/// How many compiled patterns one regular-expression engine keeps. A worker
/// gives each regex native it constructs its own engine.
#[cfg(feature = "dialect-typescript")]
const CACHED_PATTERNS: usize = 256;

/// Registers the `kernel` package's definitions: the numbers, text and
/// JSON functions, the machine's own, the collections and `brand.of`.
#[allow(
    dead_code,
    reason = "the build defines the packages; a worker only implements them"
)]
pub(crate) fn define_kernel(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_kernel_lib::register_numbers(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_text_json(registry).map_err(|error| error.to_string())?;
    lash_kernel_vm::register_machine_functions(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_collections(registry).map_err(|error| error.to_string())?;
    lash_kernel_lib::register_brands(registry).map_err(|error| error.to_string())?;
    Ok(())
}

/// Registers the `regex.ecma` package's definitions.
#[cfg(feature = "dialect-typescript")]
#[allow(
    dead_code,
    reason = "the build defines the packages; a worker only implements them"
)]
pub(crate) fn define_regex(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_ext_regex_ecma::register(registry)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Registers the `date.ecma` package's definitions.
#[cfg(feature = "dialect-typescript")]
#[allow(
    dead_code,
    reason = "the build defines the packages; a worker only implements them"
)]
pub(crate) fn define_date(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_ext_date_ecma::register(registry)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Registers the `url.whatwg` package's definitions.
#[cfg(feature = "dialect-typescript")]
#[allow(
    dead_code,
    reason = "the build defines the packages; a worker only implements them"
)]
pub(crate) fn define_url(registry: &mut FunctionRegistry) -> Result<(), String> {
    lash_ext_url_whatwg::register(registry)
        .map(|_| ())
        .map_err(|error| error.to_string())
}

/// Adds the natives and plans of the lash package at `path` to its
/// `functions`, which `registry` holds beside every package they require.
/// A package whose dialect this build was built without has none here.
#[allow(
    dead_code,
    reason = "a worker implements the packages; the build defines them"
)]
pub(crate) fn implement(
    path: &str,
    registry: &mut FunctionRegistry,
    functions: &[FunctionId],
) -> Result<(), String> {
    #[cfg(not(feature = "dialect-typescript"))]
    let _ = functions;
    match path {
        "kernel" => {
            lash_kernel_lib::implement_numbers(registry).map_err(|error| error.to_string())?;
            lash_kernel_lib::implement_text_json(registry).map_err(|error| error.to_string())?;
            lash_kernel_vm::implement_machine_functions(registry)
                .map_err(|error| error.to_string())?;
            lash_kernel_lib::implement_brands(registry).map_err(|error| error.to_string())?;
            lash_kernel_lib::implement_collections(registry).map_err(|error| error.to_string())?;
            implement_retained(registry)
        }
        #[cfg(feature = "dialect-typescript")]
        "regex.ecma" => lash_ext_regex_ecma::implement(
            registry,
            &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(CACHED_PATTERNS)),
        )
        .map(|_| ())
        .map_err(|error| error.to_string()),
        #[cfg(feature = "dialect-typescript")]
        "date.ecma" => lash_ext_date_ecma::implement(registry, functions.iter().copied())
            .map_err(|error| error.to_string()),
        #[cfg(feature = "dialect-typescript")]
        "url.whatwg" => lash_ext_url_whatwg::implement(registry, functions.iter().copied())
            .map_err(|error| error.to_string()),
        #[cfg(feature = "dialect-typescript")]
        "typescript" => lash_ext_ts_core::implement(registry, functions.iter().copied())
            .map_err(|error| error.to_string()),
        #[cfg(feature = "dialect-python")]
        "python" => lash_ext_py_core::implement(registry, functions.iter().copied())
            .map_err(|error| error.to_string()),
        _ => Ok(()),
    }
}

/// Adds the retained native of every archived kernel-library definition the
/// registry holds (ADR 0149, `K-LIB-011`): a definition binds the native
/// version it states, and archived helpers still call the superseded ones.
#[allow(
    dead_code,
    reason = "a worker implements the packages; the build defines them"
)]
fn implement_retained(registry: &mut FunctionRegistry) -> Result<(), String> {
    let archived: Vec<_> = registry
        .iter()
        .filter(|(_, registered)| {
            let definition = registered.definition();
            definition.has_native()
                && registered.native().is_none()
                && definition.native_version != lash_kernel_lib::native_version(registered.name())
        })
        .map(|(function, registered)| {
            (
                *function,
                registered.name().to_owned(),
                registered.definition().native_version,
            )
        })
        .collect();
    for (function, name, version) in archived {
        if let Some(native) = lash_kernel_lib::retained_native(&name, version) {
            registry
                .implement(function, native)
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
}
