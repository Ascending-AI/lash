//! Native construction shared by standalone engines and workers.
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_value::names::FunctionId;
#[cfg(feature = "dialect-typescript")]
const CACHED_PATTERNS: usize = 256;

/// Constructs the implementation of `function`, selected by a generated
/// identity table. This path never constructs or registers definitions.
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
        #[cfg(feature = "dialect-python")]
        "python" => lash_ext_py_core::native(name),
        _ => None,
    };
    native.unwrap_or_else(|| unreachable!("the source owner's native table names {package}:{name}"))
}
