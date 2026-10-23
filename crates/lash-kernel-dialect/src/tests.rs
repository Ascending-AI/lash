use lash_kernel_doc::core_support::RegisteredFunctionCoreSupport;
use lash_kernel_doc::{Implementation, parse_definition};

use crate::{Library, NamedLibrary, SourceError, define_functions};

/// FIG-6671: authored metadata cannot carry a second executable start
/// signature, through either the worker JSON or stored token boundary.
#[test]
fn written_refuses_a_second_start_signature() {
    use lash_kernel_doc::SourceLanguage;
    use lash_vocabulary::tokens::{TokenCodec as _, TokenWriter, decode_body};
    use lash_vocabulary::{Signature, Type};

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
    out.seq(5);
    SourceLanguage::Python.encode(&mut out);
    out.text("() -> str");
    Some(signature).encode(&mut out);
    None::<crate::CallShape>.encode(&mut out);
    None::<lash_vocabulary::SignatureOrigin>.encode(&mut out);
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
    let definitions = define_functions(HELPERS, &mut library, &[]).unwrap();
    assert_eq!(definitions.len(), 2);
    let twice = library.resolve("demo.twice").unwrap();
    assert_eq!(twice, definitions[0].identity());
    let Implementation::Guest { body, .. } = &definitions[1].implementation else {
        panic!("a helper has a body");
    };
    assert!(body.functions.contains_key(&twice));
}

/// A body's function list is part of its identity, so a name the header
/// lists and the body does not call is not in it.
#[test]
fn a_definition_lists_only_the_functions_its_body_calls() {
    let mut library = kernel_library();
    let definitions = define_functions(HELPERS, &mut library, &[]).unwrap();
    let Implementation::Guest { body, .. } = &definitions[0].implementation else {
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
    let error = define_functions(HELPERS, &mut library, &[]).unwrap_err();
    assert!(
        matches!(&error, SourceError::Parse { function, .. } if function == "demo.twice"),
        "{error}"
    );
}

/// A library is one function per name.
#[test]
fn a_name_is_defined_once() {
    let mut library = kernel_library();
    define_functions(HELPERS, &mut library, &[]).unwrap();
    let error = define_functions(HELPERS, &mut library, &[]).unwrap_err();
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

/// FIG-7135: a helper's local import slot never selects a root operation.
#[test]
fn terminal_readback_projects_each_units_exact_import_contract() {
    use lash_kernel_doc::{
        ActExpr, ActKind, Document, Expr, FunctionBody, Literal, Metering, NumberPolicy, Stmt,
    };
    use lash_vocabulary::{
        EffectName, Import, ImportKind, ImportSlot, Name, PackagePath, QualifiedName, Segment,
        Signature, Type,
    };
    use std::collections::BTreeMap;

    let import = |member: &str| Import {
        name: QualifiedName {
            package: PackagePath::parse("corpus").unwrap(),
            member: Segment::new(member).unwrap(),
            type_member: None,
        },
        kind: ImportKind::Operation {
            signature: Signature {
                params: vec![],
                result: Type::Any,
            },
        },
    };
    let ordinary = import("echo");
    let terminal = import("stop");
    let mut document = Document::new(NumberPolicy::Float, vec![]);
    document.imports = BTreeMap::from([
        (ImportSlot(0), ordinary.clone()),
        (ImportSlot(7), terminal.clone()),
    ]);
    let mut function =
        parse_definition("function helper.stop() -> Any\nkernel 1\ncharge 1\nnative\n").unwrap();
    function.implementation = Implementation::Guest {
        metering: Metering::Statements,
        body: FunctionBody {
            functions: BTreeMap::new(),
            imports: BTreeMap::from([(ImportSlot(0), terminal)]),
            block: vec![Stmt::Finish {
                value: Expr::Let {
                    name: Name::new("t1"),
                    value: Box::new(Expr::Act(Box::new(ActExpr {
                        kind: ActKind::Perform {
                            slot: ImportSlot(0),
                            result: Type::Any,
                            then_yield: true,
                        },
                        args: vec![],
                    }))),
                    body: Box::new(Expr::Literal(Literal::Null)),
                },
            }],
        },
    };
    document
        .functions
        .insert(function.identity(), function.clone());
    let namespaces = crate::regions::namespaces(&document).unwrap();
    assert!(
        namespaces
            .controls(&EffectName::new("corpus.echo").unwrap())
            .is_none()
    );
    assert!(
        namespaces
            .controls(&EffectName::new("corpus.stop").unwrap())
            .is_some()
    );

    // Equal names with a different signature are not the same import contract.
    let Implementation::Guest { body, .. } = &mut function.implementation else {
        unreachable!()
    };
    let mut mismatched = ordinary;
    mismatched.kind = ImportKind::Operation {
        signature: Signature {
            params: vec![],
            result: Type::Text,
        },
    };
    body.imports.insert(ImportSlot(0), mismatched);
    document.functions.clear();
    document.functions.insert(function.identity(), function);
    let namespaces = crate::regions::namespaces(&document).unwrap();
    assert!(
        namespaces
            .controls(&EffectName::new("corpus.echo").unwrap())
            .is_none()
    );
    assert!(
        namespaces
            .controls(&EffectName::new("corpus.stop").unwrap())
            .is_none()
    );
}

/// Build helpers contribute exact references and requirements, never authored
/// definitions or package imports, including their native dependency closure.
#[test]
fn helper_free_documents_preserve_exact_library_references() {
    let mut library = kernel_library();
    let helpers = define_functions(HELPERS, &mut library, &[]).unwrap();
    let function = helpers[1].identity();
    let mut document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    crate::authoring::include_library_dependencies(&mut document, [function], &library).unwrap();
    assert!(
        document.functions.is_empty(),
        "build helpers must not become authored definitions"
    );
    assert!(
        document.imports.is_empty(),
        "helper calls need no package import"
    );
    assert!(document.library.contains(&function));
    assert_eq!(
        document.library.iter().copied().collect::<Vec<_>>(),
        [function]
    );
}

/// FIG-7253 / D2: an operation declaration must never change literal text.
#[test]
fn helper_operation_resolution_preserves_literal_values() {
    let mut library = NamedLibrary::new();
    let definitions = define_functions(
        "function demo.literal() -> Text\nkernel 1\ncharge 1\n\
         body { return \"perform python.clock(\" }\n",
        &mut library,
        &[crate::authoring::HelperOperation {
            slot: lash_vocabulary::ImportSlot(0),
            name: crate::service_name("python", "clock"),
            requirement: lash_vocabulary::OperationRequirement::new(
                lash_vocabulary::Signature {
                    params: vec![],
                    result: lash_vocabulary::Type::Timestamp,
                },
                lash_vocabulary::ContractId::new("fixture.clock"),
            ),
        }],
    )
    .unwrap();
    let expected = parse_definition(
        "function demo.literal() -> Text\nkernel 1\ncharge 1\n\
         body { return \"perform python.clock(\" }\n",
    )
    .unwrap();
    assert_eq!(definitions[0], expected);
}

/// FIG-7253 / D11: helper operations are supplied by their owner, without a
/// shipped-package inventory. Resolution and dependency lifting preserve slots.
#[test]
fn helper_operations_compose_owner_declarations_without_package_policy() {
    use crate::authoring::{HelperOperation, Imports, include_library_dependencies};
    use lash_vocabulary::{ContractId, ImportSlot, OperationRequirement, Signature, Type};
    let operations = [HelperOperation {
        slot: ImportSlot(0),
        name: crate::service_name("third_party", "read"),
        requirement: OperationRequirement::new(
            Signature {
                params: vec![],
                result: Type::Text,
            },
            ContractId::new("third_party.read"),
        ),
    }];
    let mut library = NamedLibrary::new();
    let definitions = define_functions(
        "function demo.read() -> Text\nkernel 1\nmetering statements\nbody { let value = perform third_party.read() as Text return value }\n",
        &mut library,
        &operations,
    ).unwrap();
    let function = definitions[0].identity();
    let mut document = lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::Float, vec![]);
    let imports = Imports::default().reserving(operations.iter().map(|operation| operation.slot));
    let authored = imports.operation(
        crate::service_name("customer", "write"),
        Signature {
            params: vec![],
            result: Type::Null,
        },
    );
    assert_ne!(authored, operations[0].slot);
    document.imports = imports.into_map();
    include_library_dependencies(&mut document, [function], &library).unwrap();
    let expected = crate::authoring::operation_imports(&operations).unwrap();
    assert_eq!(
        document.imports.get(&ImportSlot(0)),
        expected.get(&ImportSlot(0))
    );
    assert_eq!(definitions[0].body().unwrap().imports, expected);
    let unknown = define_functions(
        "function demo.unknown() -> Text\nkernel 1\nmetering statements\nbody { return perform third_party.missing() as Text }\n",
        &mut library, &operations,
    ).unwrap_err();
    assert!(matches!(
        unknown,
        SourceError::Parse {
            reason: lash_kernel_doc::ParseErrorReason::UnknownOperation { .. },
            ..
        }
    ));
    document
        .imports
        .insert(ImportSlot(0), document.imports[&authored].clone());
    assert!(include_library_dependencies(&mut document, [function], &library).is_err());
}
