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
    native_version: u32,
    function: FunctionId,
    callees: &[(FunctionId, &str)],
    registry: &FunctionRegistry,
) -> std::sync::Arc<dyn lash_kernel_doc::NativeFunction> {
    implementation(package, name, native_version, function, callees, registry).unwrap_or_else(
        || {
            unreachable!(
                "the source owner's native table names {package}:{name} version {native_version}"
            )
        },
    )
}

pub(crate) fn implementation(
    package: &str,
    name: &str,
    native_version: u32,
    function: FunctionId,
    callees: &[(FunctionId, &str)],
    registry: &FunctionRegistry,
) -> Option<std::sync::Arc<dyn lash_kernel_doc::NativeFunction>> {
    #[cfg(not(feature = "dialect-typescript"))]
    let _ = (registry, callees, function);
    // These version cases belong to Rust implementations, never declarations.
    match (package, native_version) {
        ("kernel", 1) if name == "json.encode" => Some(lash_kernel_lib::json_encode_v1()),
        ("kernel", version) if version == if name == "json.encode" { 2 } else { 1 } => {
            lash_kernel_lib::native(name).or_else(|| lash_kernel_vm::machine_native(name))
        }
        #[cfg(feature = "dialect-typescript")]
        ("regex.ecma", 1) => lash_ext_regex_ecma::native(
            name,
            &std::sync::Arc::new(lash_ext_regex_ecma::Engine::new(CACHED_PATTERNS)),
            function,
        ),
        #[cfg(feature = "dialect-typescript")]
        ("date.ecma", 1) => lash_ext_date_ecma::native(name, function),
        #[cfg(feature = "dialect-typescript")]
        ("url.whatwg", 1) => lash_ext_url_whatwg::native(name, function),
        #[cfg(feature = "dialect-typescript")]
        ("typescript", version)
            if version
                == if matches!(name, "ts.lt" | "ts.le" | "ts.gt" | "ts.ge") {
                    2
                } else {
                    1
                } =>
        {
            lash_ext_ts_core::native(name, |name| {
                let (function, _) = callees.iter().find(|(_, label)| *label == name)?;
                registry.get(function)?.native().cloned()
            })
        }
        #[cfg(feature = "dialect-python")]
        ("python", 1) => lash_ext_py_core::native(name),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An unknown native version must never select the current implementation.
    #[test]
    fn versioned_native_does_not_fall_back_to_current() {
        let registry = FunctionRegistry::declarations();
        let function = FunctionId::from_bytes([0; 32]);
        assert!(implementation("kernel", "same", 1, function, &[], &registry).is_some());
        assert!(implementation("kernel", "same", 2, function, &[], &registry).is_none());
    }

    /// FIG-7639: a superseded version that archived definitions bind keeps
    /// its own answer. json.encode version 1 reads a six-field style, which
    /// the current version 2 refuses.
    #[test]
    fn a_retained_native_version_answers_with_its_own_implementation() {
        let archive = lash_kernel_releases::helper_archive().unwrap();
        let id = archive
            .releases
            .iter()
            .flat_map(|release| &release.natives)
            .find(|(_, native)| {
                native.package == "kernel"
                    && native.name == "json.encode"
                    && native.native_version == 1
            })
            .map(|(id, _)| *id)
            .unwrap();
        let engine = crate::Engine::new(crate::Limits::default()).unwrap();
        let document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers by_spelling\nlibrary @{id}\nmain {{ let v = invoke @{id}([1, \"é\"], (0, 21, false, false), (true, \",\", \":\", 2, false, (\"a\", \"b\", \"c\"))) return v }}"
        ))
        .unwrap();
        let link = lash_kernel_value::Link {
            helper_fills: Default::default(),
            document: document.identity(),
            fills: Default::default(),
        };
        let content = lash_kernel_package::LinkedContent {
            documents: [(document.identity(), std::sync::Arc::new(document))].into(),
            links: [(link.identity(), link.clone())].into(),
            packages: Default::default(),
        };
        let program = engine
            .prepare(link.identity(), &content, lash_kernel_vm::Target::Main)
            .unwrap();
        let crate::Progress::Complete(Ok(value)) = engine.run(&program, vec![]).unwrap().state
        else {
            panic!("json.encode version 1 did not complete");
        };
        assert_eq!(value.data(), &crate::Datum::Text("[1,\"\\u00e9\"]".into()));
    }
}
