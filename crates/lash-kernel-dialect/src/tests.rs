use lash_kernel_doc::core_support::RegisteredFunctionCoreSupport;
use lash_kernel_doc::{Implementation, parse_definition};

use crate::{Library, NamedLibrary, SourceError, define_functions};

/// FIG-6671: authored metadata cannot carry a second executable start
/// signature, through either the worker JSON or stored token boundary.
#[test]
fn written_refuses_a_second_start_signature() {
    use lash_kernel_doc::graph_tokens::{TokenCodec as _, TokenWriter, decode_body};
    use lash_kernel_doc::{Signature, SourceLanguage, Type};

    let signature = Signature {
        params: Vec::new(),
        result: Type::Int,
    };
    let data = serde_json::json!({
        "language": SourceLanguage::Python,
        "signature": "() -> str",
        "start": signature,
    });
    let refused = serde_json::from_value::<crate::Written>(data)
        .expect_err("authored metadata cannot claim an executable start");
    assert!(refused.to_string().contains("unknown field `start`"));

    let mut out = TokenWriter::new();
    out.seq(4);
    SourceLanguage::Python.encode(&mut out);
    out.text("() -> str");
    Some(signature).encode(&mut out);
    None::<crate::CallShape>.encode(&mut out);
    assert!(decode_body::<crate::Written>(&out.into_bytes()).is_err());
}

fn kernel_library() -> NamedLibrary {
    let mut library = NamedLibrary::new();
    for name in ["num.add", "num.neg"] {
        let text = format!("function {name}(a: Any, b?: Any) -> Any\nkernel 1\ncharge 1\nnative\n");
        library.insert(parse_definition(&text).unwrap()).unwrap();
    }
    library
}

const HELPERS: &str = "\
# The header names what the bodies call.
use num.add
use num.neg
use demo.twice

function demo.twice(x: Any) -> Any
kernel 1
charge 2
body {
  return num.add(x, x)
}

function demo.minus_twice(x: Any) -> Any
kernel 1
charge 4
body {
  let doubled = invoke demo.twice(x)
  return num.neg(doubled)
}
";

/// A helper source names functions, the library gives the identities, and a
/// later helper calls an earlier one.
#[test]
fn a_helper_source_is_resolved_by_name_in_order() {
    let mut library = kernel_library();
    let definitions = define_functions(HELPERS, &mut library).unwrap();
    assert_eq!(definitions.len(), 2);
    let twice = library.resolve("demo.twice").unwrap();
    assert_eq!(twice, definitions[0].identity());
    let Implementation::Body(body) = &definitions[1].implementation else {
        panic!("a helper has a body");
    };
    assert!(body.functions.contains_key(&twice));
}

/// A body's function list is part of its identity, so a name the header
/// lists and the body does not call is not in it.
#[test]
fn a_definition_lists_only_the_functions_its_body_calls() {
    let mut library = kernel_library();
    let definitions = define_functions(HELPERS, &mut library).unwrap();
    let Implementation::Body(body) = &definitions[0].implementation else {
        panic!("a helper has a body");
    };
    let names: Vec<&str> = body.functions.values().map(|name| name.as_str()).collect();
    assert_eq!(names, ["num.add"]);
}

/// A body that calls a name the library does not hold is refused, naming
/// the helper.
#[test]
fn a_call_to_a_name_the_library_lacks_names_the_helper() {
    let mut library = NamedLibrary::new();
    let error = define_functions(HELPERS, &mut library).unwrap_err();
    assert!(
        matches!(&error, SourceError::Parse { function, .. } if function == "demo.twice"),
        "{error}"
    );
}

/// A library is one function per name.
#[test]
fn a_name_is_defined_once() {
    let mut library = kernel_library();
    define_functions(HELPERS, &mut library).unwrap();
    let error = define_functions(HELPERS, &mut library).unwrap_err();
    assert!(matches!(error, SourceError::Library(_)), "{error}");
}

/// FIG-6255: name resolution retains registration facts, sharing derivation
/// with the worker's registry rather than identifying and checking helpers again.
#[test]
fn named_libraries_share_registered_definition_facts() {
    use lash_kernel_doc::{FunctionCatalog, FunctionRegistry, parse_definition};
    let mut registry = FunctionRegistry::declarations();
    let function = registry
        .register(
            parse_definition("function helper() -> Int\nkernel 1\ncharge 1\nbody { return 1 }\n")
                .unwrap(),
            None,
        )
        .unwrap();
    let first = NamedLibrary::from_registry(&registry).unwrap();
    let resolving = NamedLibrary::resolving(&registry, |_| true).unwrap();
    let calls = std::cell::Cell::new(0);
    let derive = || {
        calls.set(calls.get() + 1);
        42usize
    };
    let registered = registry.get(&function).unwrap();
    let result = registered.derive(derive);
    for library in [&first, &resolving] {
        let held = library.registered(&function).unwrap();
        assert_eq!(held.identity(), function);
        assert!(std::sync::Arc::ptr_eq(&held.derive(derive), &result));
        assert!(std::ptr::eq(
            library
                .registration(library.definition(&function).unwrap())
                .unwrap(),
            held
        ));
    }
    assert_eq!(calls.get(), 1);
}
