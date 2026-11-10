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
        ("kernel", version) if version == lash_kernel_lib::native_version(name) => {
            lash_kernel_lib::native(name).or_else(|| lash_kernel_vm::machine_native(name))
        }
        ("kernel", version) => lash_kernel_lib::retained_native(name, version),
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
                == if matches!(name, "ts.add" | "ts.lt" | "ts.le" | "ts.gt" | "ts.ge") {
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

    /// Runs `main` over the archived version 1 declaration of kernel `name`,
    /// whose call is `invoke @<id>(<args>)` after `lets`.
    fn run_version_1(name: &str, lets: &str, args: &str) -> Result<crate::Datum, (String, String)> {
        let archive = lash_kernel_releases::helper_archive().unwrap();
        let id = archive
            .releases
            .iter()
            .flat_map(|release| &release.natives)
            .find(|(_, native)| {
                native.package == "kernel" && native.name == name && native.native_version == 1
            })
            .map(|(id, _)| *id)
            .unwrap_or_else(|| panic!("no archived kernel:{name} version 1"));
        let engine = crate::Engine::new(crate::Limits::default()).unwrap();
        let document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers by_spelling\nlibrary @{id}\nmain {{ {lets} let v = invoke @{id}({args}) return v }}"
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
        match engine.run(&program, vec![]).unwrap().state {
            crate::Progress::Complete(Ok(value)) => Ok(value.data().clone()),
            crate::Progress::Complete(Err(crate::CompletionError::Run(
                crate::RunError::Uncaught {
                    error: crate::Datum::Error(error),
                    ..
                },
            ))) => Err((error.kind.clone(), error.message.clone())),
            _ => panic!("kernel:{name} version 1 neither answered nor raised"),
        }
    }

    /// FIG-7639, FIG-7640: a superseded version that archived definitions
    /// bind keeps its own answer. The expected answers follow the writer as
    /// it was before FIG-7637 (0f8721081a^): json.encode version 1 reads a
    /// six-field style, which version 2 refuses; json.render and
    /// json.stringify version 1 refuse without a JSON path, where version 2
    /// names one (`json $: ...`, `json $[1]: ...`). json.classify version 1
    /// takes a caller depth (FIG-7656): at depth 8 it visits nine nested
    /// lists and refuses the ninth, answering `2 * 9 + 0`, where version 2
    /// takes no depth.
    #[test]
    fn a_retained_native_version_answers_with_its_own_implementation() {
        assert_eq!(
            run_version_1(
                "json.encode",
                "",
                "[1, \"é\"], (0, 21, false, false), (true, \",\", \":\", 2, false, (\"a\", \"b\", \"c\"))"
            ),
            Ok(crate::Datum::Text("[1,\"\\u00e9\"]".into()))
        );
        assert_eq!(
            run_version_1(
                "json.render",
                "let m = map { 1: 2 } let kinds = set { }",
                "m, kinds, absent, (0, 21, false, false)"
            ),
            Err(("json_key".into(), "JSON map keys must be text".into()))
        );
        assert_eq!(
            run_version_1("json.stringify", "let l = [1, nan]", "l"),
            Err(("json_number".into(), "JSON refuses NaN and infinity".into()))
        );
        assert_eq!(
            run_version_1(
                "json.classify",
                "",
                "[[[[[[[[[1]]]]]]]]], 8, (\"toJSON\", \"__lash_brand\"), \"0\", \":\", false"
            ),
            Ok(crate::Datum::Int(18i64.into()))
        );
    }
}
