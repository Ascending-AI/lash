//! Non-output observations of the corpus's document and definition rules.
//! Each probe runs with its owning shard; these are not extra coverage rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_check::{Environment, RefusalReason, admit, derive, derive_definition};
use lash_kernel_doc::Document;
use lash_kernel_doc::FunctionDefinition;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::Implementation;
use lash_kernel_doc::InvalidReason;
use lash_kernel_doc::NativeCall;
use lash_kernel_doc::NativeError;
use lash_kernel_doc::NativeFunction;
use lash_kernel_doc::Node;
use lash_kernel_doc::SourceLanguage;
use lash_kernel_doc::SourceMap;
use lash_kernel_doc::SourceSite;
use lash_kernel_doc::Span;
use lash_kernel_doc::Value;
use lash_kernel_doc::parse_definition;
use lash_kernel_doc::parse_document;
use lash_kernel_doc::print_definition;
use lash_kernel_doc::print_document;
use lash_kernel_doc::validate_definition;
use lash_kernel_doc::validate_document;
use lash_kernel_value::names::Name;
use lash_kernel_value::sites::Site;
use lash_kernel_value::sites::Unit;
use lash_kernel_vm::kernel_internals::Layout;
use lash_kernel_vm::{
    CellStart, DeliverError, Delivered, End, KernelMachine, Machine, Outcome, PreparedLibrary,
    Program, Request, Step,
};

use crate::{Case, ExpectedEnd, MachineRunner, Shard, check_case};

const HEADER: &str = "kernel 1\nnumbers by_spelling\n";
const NATIVE: &str = "function probe.same(x: Any) -> Any\nkernel 1\ncharge 5\nnative\n";
const BODY: &str = "function probe.same(x: Any) -> Any\nkernel 1\ncharge 5\nbody { return x }\n";
const BOTH: &str =
    "function probe.same(x: Any) -> Any\nkernel 1\ncharge 5\nnative\nbody { return x }\n";

struct Same;
impl NativeFunction for Same {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        call.counter.spend(1)?;
        Ok(call.args.values()[0])
    }
}

fn definition(text: &str) -> FunctionDefinition {
    parse_definition(text).expect("parse the rule's definition")
}

fn doc(text: &str) -> Document {
    parse_document(text).expect("parse the rule's document")
}

fn calling(definitions: &[FunctionDefinition], main: &str) -> Document {
    let mut source = HEADER.to_owned();
    for definition in definitions {
        source.push_str(&format!(
            "definition {}\n",
            serde_json::to_string(&print_definition(definition)).unwrap()
        ));
        if definition.has_native() {
            source.push_str(&format!("native @{}\n", definition.identity()));
        }
    }
    source.push_str(main);
    doc(&source)
}

fn same_registry(text: &str, native: bool) -> FunctionRegistry {
    let mut registry = FunctionRegistry::new();
    registry
        .register(
            definition(text),
            native.then(|| Arc::new(Same) as Arc<dyn NativeFunction>),
        )
        .expect("register rule fixture");
    registry
}

pub(super) fn verify(shard: &Shard, registry: &FunctionRegistry) {
    match shard.rule.as_str() {
        "K-DOC-001" => {
            let document = doc(&shard.cases[0].document);
            let graph = derive(&document, registry).unwrap();
            assert!(document.private_bindings.contains(&Name::new("scratch")));
            for node in graph.nodes() {
                assert!(document.node(&node.site).is_some(), "{}", node.site);
            }
            let mut encoded: serde_json::Value =
                serde_json::from_str(&document.to_json().unwrap()).unwrap();
            for field in ["nodes", "edges", "scopes", "execution_sites"] {
                encoded[field] = serde_json::json!([]);
                assert!(Document::from_json(&encoded.to_string()).is_err());
                encoded.as_object_mut().unwrap().remove(field);
            }
        }
        "K-ADM-007" => manifest_closure(),
        "K-DOC-004" => stored_encoding(shard),
        "K-DOC-005" => {
            let document = doc(&format!("{HEADER}main {{ break }}"));
            let invalid = validate_document(&document, registry).unwrap_err();
            assert_eq!(invalid.site, Some(Site::new(Unit::Main, [0])));
            assert!(matches!(
                *invalid.reason,
                InvalidReason::LoopControlOutsideLoop { .. }
            ));
        }
        "K-ADM-009" => source_map_independence(shard, registry),
        "K-ID-001" | "K-ID-003" => identity_document(shard),
        "K-ID-002" => identity_definition(),
        "K-LIB-001" => definition_parts(),
        "K-LIB-002" => implementations(),
        "K-LIB-003" => callbacks(),
        "K-LIB-004" => body_rules(),
        "K-LIB-005" => {
            let mut registry = FunctionRegistry::new();
            assert!(matches!(
                registry.register(definition(NATIVE), None),
                Err(lash_kernel_doc::RegistryError::NativeMissing { .. })
            ));
            assert!(matches!(
                registry.register(definition(BODY), Some(Arc::new(Same))),
                Err(lash_kernel_doc::RegistryError::NativeNotStated { .. })
            ));
        }
        "K-LIB-009" => adoption(),
        "K-TEXT-001" => text_roundtrip(shard),
        "K-VER-001" => {
            let mut definition = definition(NATIVE);
            definition.kernel = 99;
            assert!(matches!(
                *validate_definition(&definition, registry)
                    .unwrap_err()
                    .reason,
                InvalidReason::KernelVersion { found: 99, .. }
            ));
        }
        "K-ADM-003" => missing_transitive(),
        "K-ADM-005" | "K-ADM-006" => typed_refusals(shard, registry),
        "K-ADM-008" => reached_body_rules(),
        "K-SITE-004" => enclosing_loops(shard, registry),
        "K-SITE-005" => site_order(shard, registry),
        "K-CHG-001" => charge_invariance(shard, registry),
        "K-MACH-001" => instance_isolation(shard, registry),
        "K-MACH-004" => delivery_errors(shard, registry),
        rule if rule.starts_with("K-CAP-") => super::captions::verify(rule),
        _ => {}
    }
}

fn definition_parts() {
    let text = "function probe.same(x: Any) -> Any\nkernel 1\nerrors \"probe.error\"\ncharge sum(5, size(x))\nguard \"step\" 9\nnative\n";
    let original = definition(text);
    assert_eq!(original.kernel, 1);
    assert_eq!(original.name.as_str(), "probe.same");
    assert_eq!(original.signature.params[0].name.as_str(), "x");
    assert!(original.errors.contains("probe.error"));
    assert_eq!(original.guard.as_ref().unwrap().unit, "step");
    assert!(matches!(
        original.implementation,
        Implementation::Native { .. }
    ));
    assert_eq!(
        FunctionDefinition::from_json(&original.to_json().unwrap()).unwrap(),
        original
    );
    assert_eq!(definition(&print_definition(&original)), original);
}

fn implementations() {
    for text in [NATIVE, BODY, BOTH] {
        let original = definition(text);
        let catalog = same_registry(text, text != BODY);
        let id = original.identity();
        let expression = calling(
            std::slice::from_ref(&original),
            "main { finish probe.same(7) }",
        );
        let result = validate_document(&expression, &catalog);
        if text == BODY {
            assert!(
                matches!(*result.unwrap_err().reason, InvalidReason::CallNotNative { function, .. } if function == id)
            );
        } else {
            result.unwrap();
        }
        let statement = calling(
            &[original],
            "main { let x = invoke probe.same(7) finish x }",
        );
        admit(&statement, &Environment::new(&catalog)).unwrap();
    }
}

fn callbacks() {
    let catalog = FunctionRegistry::new();
    for ty in [
        "Fn(x: Any) -> Any",
        "List(Fn(x: Any) -> Any)",
        "Tuple(Fn(x: Any) -> Any)",
    ] {
        let native = definition(&format!(
            "function probe.callback(f: {ty}) -> Any\nkernel 1\ncharge 1\nnative\n"
        ));
        assert!(matches!(
            *validate_definition(&native, &catalog).unwrap_err().reason,
            InvalidReason::NativeTakesFunction { .. }
        ));
    }
    let body = definition(
        "function probe.callback(f: Fn() -> Any) -> Any\nkernel 1\ncharge 1\nbody { let x = apply f() return x }\n",
    );
    validate_definition(&body, &catalog).unwrap();
}

fn body_rules() {
    let catalog = FunctionRegistry::new();
    for (statement, native, imports) in [
        ("do perform 0(1) as Int", false, ""),
        (
            "do perform 0(1) as Int",
            true,
            "import 0 operation corpus.echo(value: Int) -> Int\n",
        ),
        ("do yield", true, ""),
    ] {
        let text = format!(
            "function probe.bad() -> Any\nkernel 1\ncharge 1\n{}{imports}body {{ {statement} }}",
            if native { "native\n" } else { "" }
        );
        let invalid = validate_definition(&definition(&text), &catalog).unwrap_err();
        if native {
            let form = if imports.is_empty() {
                lash_kernel_doc::StatementForm::Yield
            } else {
                lash_kernel_doc::StatementForm::Perform
            };
            assert!(
                matches!(*invalid.reason, InvalidReason::NativeBodyMayWait { form: found } if found == form)
            );
        } else {
            assert!(matches!(
                *invalid.reason,
                InvalidReason::Import {
                    slot: lash_kernel_value::ImportSlot(0)
                }
            ));
        }
    }
}

fn outer(inner: &FunctionDefinition) -> FunctionDefinition {
    let id = inner.identity();
    definition(&format!(
        "function probe.outer(x: Any) -> Any\nkernel 1\ncharge 5\nuse probe.same = @{id}\nbody {{ let y = invoke probe.same(x) return y }}"
    ))
}

fn manifest_closure() {
    let inner = definition(BODY);
    let outer = outer(&inner);
    let inner_id = inner.identity();
    let outer_id = outer.identity();
    let mut document = Document::new(
        lash_kernel_doc::NumberPolicy::BySpelling,
        vec![lash_kernel_doc::Stmt::Return {
            value: lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Null),
        }],
    );
    document.functions.insert(outer_id, outer);
    assert!(admit(&document, &Environment::new(&document.functions)).is_err());
    document.functions.insert(inner_id, inner);
    admit(&document, &Environment::new(&document.functions)).unwrap();
}

fn missing_transitive() {
    let inner = definition(BODY);
    let outer = outer(&inner);
    let id = outer.identity();
    let inner_id = inner.identity();
    let document = calling(
        std::slice::from_ref(&outer),
        "main { let x = invoke probe.outer(7) finish x }",
    );
    let catalog = BTreeMap::from([(id, outer)]);
    let errors = admit(&document, &Environment::new(&catalog))
        .unwrap_err()
        .errors;
    assert!(errors.iter().any(|e| matches!(e.reason, RefusalReason::Invalid(InvalidReason::FunctionNotListed { function }) if function == inner_id)));
}

fn reached_body_rules() {
    for statement in ["break", "return missing"] {
        let bad = definition(&format!(
            "function probe.bad() -> Any\nkernel 1\ncharge 1\nbody {{ {statement} }}"
        ));
        let id = bad.identity();
        let document = calling(
            std::slice::from_ref(&bad),
            "main { let x = invoke probe.bad() finish x }",
        );
        let catalog = BTreeMap::from([(id, bad)]);
        let errors = admit(&document, &Environment::new(&catalog))
            .unwrap_err()
            .errors;
        assert!(errors.iter().any(|error| {
            error
                .site
                .as_ref()
                .is_some_and(|site| site.unit == Unit::Library(id))
        }));
    }
}

fn typed_refusals(shard: &Shard, registry: &FunctionRegistry) {
    for case in &shard.cases {
        if case.expected.end != ExpectedEnd::Refused {
            continue;
        }
        let document = doc(&case.document);
        let env = Environment::new(registry);
        let errors = admit(&document, &env).unwrap_err().errors;
        assert!(errors.iter().any(|error| match shard.rule.as_str() {
            "K-ADM-005" => matches!(error.reason, RefusalReason::ArgumentType { .. }),
            _ => matches!(error.reason, RefusalReason::PerformResult { .. }),
        }));
    }
}

fn adoption() {
    let mut registry = same_registry(BODY, false);
    let first = definition(BODY);
    let corrected = definition(&BODY.replace("return x", "return 9"));
    let first_id = first.identity();
    let next_id = registry.register(corrected.clone(), None).unwrap();
    assert_ne!(first_id, next_id);
    for (definition, result) in [(first, 7), (corrected, 9)] {
        let text = print_document(&calling(
            &[definition],
            "main { let x = invoke probe.same(7) finish x }",
        ));
        let mut case = super::case("adopt a corrected library identity");
        case.document = text.clone();
        case.expected.end = ExpectedEnd::Finished(lash_kernel_doc::Datum::Int(result.into()));
        let mut runner = MachineRunner::<KernelMachine>::new(Arc::new(registry.clone()));
        check_case(&mut runner, &case).unwrap();
    }
}

/// A source map of the first statement, over `source`.
fn statement_map(document: &Document, source: &str) -> SourceMap {
    let site = SourceSite {
        site: Site::new(Unit::Main, [0]),
        location: lash_kernel_doc::SourceLocation::Spelled {
            ranges: lash_kernel_doc::NonEmptySpans::new(Span::new(0, source.len()).unwrap()),
        },
    };
    SourceMap::new(document, source, SourceLanguage::TypeScript, vec![site]).unwrap()
}

fn source_map_independence(shard: &Shard, registry: &FunctionRegistry) {
    let document = doc(&shard.cases[0].document);
    let id = document.identity();
    let graph = derive(&document, registry).unwrap();
    let map = statement_map(&document, "let x = 1 // arbitrary authored source");
    assert_eq!(map.document, id);
    let stored = Document::from_json(&document.to_json().unwrap()).unwrap();
    assert_eq!(stored.identity(), id);
    assert_eq!(derive(&stored, registry).unwrap(), graph);
    assert!(!print_document(&document).contains("arbitrary"));
    drop(map);
    assert_eq!(derive(&document, registry).unwrap(), graph);
}

fn stored_encoding(shard: &Shard) {
    let document = doc(&shard.cases[0].document);
    let json = document.to_json().unwrap();
    assert_eq!(Document::from_json(&json).unwrap(), document);
    let mut tree: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(tree.get("manifest").is_none());
    assert!(tree["functions"].is_object());
    assert!(tree["imports"].is_object());
    assert_eq!(tree["requires"]["kernels"], serde_json::json!([1]));
    assert_eq!(
        tree["main"][0]["finish"]["value"]["tuple"][0],
        serde_json::json!({"literal": {"int": "1"}})
    );
    for pointer in [
        "",
        "/requires",
        "/main/0/finish",
        "/main/0/finish/value/tuple/0/literal",
    ] {
        let object = tree.pointer_mut(pointer).unwrap().as_object_mut().unwrap();
        object.insert("unknown".into(), serde_json::json!(true));
        assert!(Document::from_json(&tree.to_string()).is_err(), "{pointer}");
        tree.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .remove("unknown");
    }
}

fn text_roundtrip(shard: &Shard) {
    for case in &shard.cases {
        let document = doc(&case.document);
        let printed = print_document(&document);
        let again = doc(&printed);
        assert_eq!(again, document);
        assert_eq!(print_document(&again), printed);
        assert!(!printed.contains('#'));
    }
    for text in [NATIVE, BODY, BOTH] {
        let original = definition(text);
        let printed = print_definition(&original);
        let again = definition(&printed);
        assert_eq!(original, again);
        assert_eq!(print_definition(&again), printed);
    }
}

fn enclosing_loops(shard: &Shard, registry: &FunctionRegistry) {
    let document = doc(&shard.cases[0].document);
    let graph = derive(&document, registry).unwrap();
    let sites = graph.execution_sites();
    let inside = sites
        .iter()
        .find(|s| s.kind == lash_kernel_check::SiteKind::Yield)
        .unwrap();
    assert_eq!(inside.loops.len(), 1);
    assert_ne!(inside.loops[0], Site::new(Unit::Main, [0]));
    let call = sites
        .iter()
        .find(|s| s.kind == lash_kernel_check::SiteKind::Call)
        .unwrap();
    assert_eq!(call.loops, [Site::new(Unit::Main, [0])]);
}

fn site_order(shard: &Shard, registry: &FunctionRegistry) {
    fn walk(document: &Document, site: Site, result: &mut Vec<Site>) {
        let node = document.node(&site).unwrap();
        result.push(site.clone());
        if let Node::Expr(lash_kernel_doc::Expr::Closure(closure)) = node {
            walk(
                document,
                Site::new(Unit::Library(closure.function), []),
                result,
            );
        }
        for (index, _) in node.children().iter().enumerate() {
            walk(document, site.child(index as u32), result);
        }
    }
    let document = doc(&shard.cases[0].document);
    let graph = derive(&document, registry).unwrap();
    let mut sites = Vec::new();
    walk(&document, Site::new(Unit::Main, []), &mut sites);
    for id in document.functions.keys() {
        let root = Site::new(Unit::Library(*id), []);
        if !sites.contains(&root) {
            walk(&document, root, &mut sites);
        }
    }
    assert_eq!(
        graph
            .nodes()
            .iter()
            .map(|n| n.site.clone())
            .collect::<Vec<_>>(),
        sites
    );
    for (index, node) in graph.nodes().iter().enumerate() {
        assert_eq!(node.id.index(), index);
    }
    assert!(graph.nodes().iter().any(|node| matches!(
        document.node(&node.site),
        Some(Node::Expr(lash_kernel_doc::Expr::Closure(_)))
    )));
    let body = definition(BODY);
    let id = body.identity();
    let library = derive_definition(&body, registry).unwrap();
    assert!(
        library
            .nodes()
            .iter()
            .all(|node| node.site.unit == Unit::Library(id))
    );
}

fn charge_invariance(shard: &Shard, registry: &FunctionRegistry) {
    let mut case = shard.cases[0].clone();
    let mut runner = MachineRunner::<KernelMachine>::new(Arc::new(registry.clone()));
    let resumed = check_case(&mut runner, &case).unwrap();
    case.environment.resume = false;
    assert_eq!(check_case(&mut runner, &case).unwrap(), resumed);
    let document = calling(
        &[definition(BOTH)],
        "main { let x = invoke probe.same(7) finish x }",
    );
    let mut observations = Vec::new();
    for native in [false, true] {
        for layout in [Layout(0), Layout(37)] {
            let program = crate::machine::prepare_document(
                &(PreparedLibrary::new(Arc::new(same_registry(BOTH, native)))),
                Arc::new(document.clone()),
                None,
            )
            .expect("prepare linked law document");
            let mut machine = crate::pages::start_with_layout(
                program,
                crate::CaseBounds::default().into(),
                start(),
                layout,
            )
            .unwrap();
            let mut host = ProbeHost;
            let end = machine.run(&mut host, u64::MAX).unwrap();
            observations.push((end, machine.meters().charged.expect("exact charge")));
        }
    }
    assert!(observations.iter().all(|item| item == &observations[0]));
}

fn instance_isolation(shard: &Shard, registry: &FunctionRegistry) {
    let program = program(&shard.cases[0], registry);
    let mut first = crate::pages::start(
        program.clone(),
        crate::CaseBounds::default().into(),
        start(),
    )
    .unwrap();
    let mut second =
        crate::pages::start(program, crate::CaseBounds::default().into(), start()).unwrap();
    let mut host = ProbeHost;
    assert_eq!(first.run(&mut host, 1).unwrap(), Step::Slice);
    let second_end = second.run(&mut host, u64::MAX).unwrap();
    let first_end = first.run(&mut host, u64::MAX).unwrap();
    assert_eq!(first_end, second_end);
    assert_eq!(
        first.meters().charged.expect("exact charge"),
        second.meters().charged.expect("exact charge")
    );
    assert_eq!(first.meters().memory, second.meters().memory);
}

fn program(case: &Case, registry: &FunctionRegistry) -> Program {
    crate::machine::prepare_document(
        &PreparedLibrary::new(Arc::new(registry.clone())),
        Arc::new(doc(&case.document)),
        None,
    )
    .expect("prepare linked law document")
}

fn delivery_errors(shard: &Shard, registry: &FunctionRegistry) {
    let mut machine = crate::pages::start(
        program(&shard.cases[0], registry),
        crate::CaseBounds::default().into(),
        start(),
    )
    .unwrap();
    let mut host = ProbeHost;
    let Step::Parked(park) = machine.run(&mut host, u64::MAX).unwrap() else {
        panic!("effect parks")
    };
    let Request::Effect(request) = &park.requests[0];
    let before = lash_kernel_value::tokens::encode_body(
        &machine
            .seal(crate::pages::BUDGET)
            .unwrap()
            .unwrap()
            .checkpoint(),
    );
    assert!(matches!(
        machine.deliver(
            lash_kernel_doc::kernel_internals::wait_id(u64::MAX),
            Outcome::Completed(lash_kernel_doc::Datum::Null)
        ),
        Err(DeliverError::UnknownWait { .. })
    ));
    assert_eq!(
        lash_kernel_value::tokens::encode_body(
            &machine
                .seal(crate::pages::BUDGET)
                .unwrap()
                .unwrap()
                .checkpoint()
        ),
        before
    );
    assert_eq!(
        machine
            .deliver(
                request.wait,
                Outcome::Completed(lash_kernel_doc::Datum::Int(2.into()))
            )
            .unwrap(),
        Delivered::Accepted
    );
    let accepted = lash_kernel_value::tokens::encode_body(
        &machine
            .seal(crate::pages::BUDGET)
            .unwrap()
            .unwrap()
            .checkpoint(),
    );
    assert!(matches!(
        machine.deliver(
            request.wait,
            Outcome::Completed(lash_kernel_doc::Datum::Int(99.into()))
        ),
        Err(DeliverError::AlreadyDelivered { .. })
    ));
    assert_eq!(
        lash_kernel_value::tokens::encode_body(
            &machine
                .seal(crate::pages::BUDGET)
                .unwrap()
                .unwrap()
                .checkpoint()
        ),
        accepted
    );
    assert!(matches!(
        machine.run(&mut host, u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
}

struct ProbeHost;
impl lash_kernel_vm::Host for ProbeHost {
    fn print(&mut self, _: &lash_kernel_doc::Datum) {
        panic!("no print in this case")
    }
    fn cancel_requested(&mut self) -> bool {
        false
    }
}

fn identity_document(shard: &Shard) {
    let document = doc(&shard.cases[0].document);
    let expected = if shard.rule == "K-ID-001" {
        "e23a05d2f50a55444c4c258e3f0516b57c4e9013c7836e56579953fa5f96032d"
    } else {
        "d2b14cd15ef5d732e857ba613e8fa9368d11fc07223b14a7cd62e93240f6a044"
    };
    assert_eq!(document.identity().to_string(), expected);
    // K-ID-003: the canonical form is the document's logical token body,
    // which decodes back to it, and the identity is that body's digest.
    let bytes = document.canonical_bytes();
    assert_eq!(
        lash_kernel_value::tokens::decode_body::<Document>(&bytes).unwrap(),
        document
    );
    assert_eq!(
        document.identity().as_bytes(),
        &lash_kernel_value::tokens::logical_digest(&bytes)
    );
    if shard.rule == "K-ID-001" {
        // Two sources mapped onto one document share its identity.
        let first = statement_map(&document, "return 7");
        let second = statement_map(&document, "finish(7)  # another spelling");
        assert_ne!(first.source, second.source);
        let id = document.identity();
        assert_eq!((first.document, second.document), (id, id));
    }
    let different = doc(&format!("{HEADER}main {{ finish 8 }}"));
    assert_ne!(document.identity(), different.identity());
}

fn identity_definition() {
    let original = definition(NATIVE);
    assert_eq!(
        original.identity().to_string(),
        "c5fa3169db03519a6c9ab76f0c2f387f0bfd4cacb4525c61f91f31c47a1c6838"
    );
    let first = original.identity();
    let json: serde_json::Value = serde_json::from_str(&original.to_json().unwrap()).unwrap();
    for (field, value) in [
        ("name", serde_json::json!("probe.other")),
        (
            "signature",
            serde_json::json!({"params": [{"name": "x", "ty": "int"}], "result": "any"}),
        ),
        ("errors", serde_json::json!(["probe.error"])),
        (
            "/implementation/native/charge",
            serde_json::json!({"constant": 6}),
        ),
        (
            "guard",
            serde_json::json!({"unit": "step", "limit": {"constant": 9}}),
        ),
        (
            "implementation",
            serde_json::to_value(definition(BODY).implementation).unwrap(),
        ),
    ] {
        let mut changed = json.clone();
        if field.starts_with('/') {
            *changed.pointer_mut(field).unwrap() = value;
        } else {
            changed[field] = value;
        }
        let changed = FunctionDefinition::from_json(&changed.to_string()).unwrap();
        assert_ne!(changed.identity(), first, "{field}");
        assert_eq!(changed.kernel, 1);
    }
}

fn start() -> CellStart {
    CellStart {
        target: lash_kernel_vm::Target::Main,
        args: Vec::new(),
        session: None,
        owner: crate::pages::OWNER,
        modes: Default::default(),
        host: Vec::new(),
    }
}
