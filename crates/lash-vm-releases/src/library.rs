//! lash's own packages (FIG-6917): each one's definitions, as the crate that
//! implements it registers them, and its natives, by the package's path.
//! Every embedding holds `kernel`; `regex.ecma`, `date.ecma`, `url.whatwg`
//! and `typescript` are the TypeScript dialect's (FIG-5990), and `python`
//! the Python dialect's. Each dialect's hot operations have partial natives
//! in a dialect-free crate (FIG-6440).
//!
//! This crate's build script includes this file, and so does
//! `lash-vm-worker`: the build makes each package once, when this crate is
//! built, and a worker adds the natives of the packages its parent holds,
//! by identity. The library of this crate holds data alone, and does not
//! compile this file.

use lash_kernel_doc::FunctionRegistry;
use lash_vocabulary::names::FunctionId;

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
            lash_kernel_lib::implement_collections(registry).map_err(|error| error.to_string())
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

/// Constructs the implementation of `function`, selected by a generated
/// identity table. This path never constructs or registers definitions.
#[allow(
    dead_code,
    reason = "used by the worker, which shares this source with the build"
)]
pub(crate) fn native(
    package: &str,
    name: &str,
    function: FunctionId,
    callees: &[(FunctionId, &str)],
    registry: &FunctionRegistry,
) -> std::sync::Arc<dyn lash_kernel_doc::NativeFunction> {
    #[cfg(not(feature = "dialect-typescript"))]
    let _ = (registry, callees, function);
    let native = match package {
        "kernel" => lash_kernel_lib::native(name).or_else(|| lash_kernel_vm::machine_native(name)),
        #[cfg(feature = "dialect-typescript")]
        "regex.ecma" => lash_ext_regex_ecma::native(
            name,
            &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(CACHED_PATTERNS)),
            function,
        ),
        #[cfg(feature = "dialect-typescript")]
        "date.ecma" => lash_ext_date_ecma::native(name, function),
        #[cfg(feature = "dialect-typescript")]
        "url.whatwg" => lash_ext_url_whatwg::native(name, function),
        #[cfg(feature = "dialect-typescript")]
        "typescript" => lash_ext_ts_core::native(name, |name| {
            let (function, _) = callees.iter().find(|(_, label)| *label == name)?;
            registry.get(function)?.native().cloned()
        }),
        _ => None,
    };
    native.unwrap_or_else(|| unreachable!("the source owner's native table names {package}:{name}"))
}
