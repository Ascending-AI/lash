//! Non-output observations of the corpus's document and definition rules.
//! Each probe runs with its owning shard; these are not extra coverage rows.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_check::{
    Environment, RefusalReason, admit, derive, derive_definition, requirements,
};
use lash_kernel_doc::{
    Annotations, Document, FunctionDefinition, FunctionRegistry, Implementation, InvalidReason,
    Label, Name, NativeCall, NativeError, NativeFunction, Node, NodeAnnotation, Site, Unit, Value,
    parse_definition, parse_document, print_definition, print_document, validate_annotations,
    validate_definition, validate_document,
};
use lash_kernel_vm::{
    DeliverError, Delivered, End, KernelMachine, Layout, Machine, Outcome, PreparedLibrary,
    Program, Request, Start, Step, WaitId,
};

use crate::{Case, ExpectedEnd, MachineRunner, Shard, check_case};

const HEADER: &str = "kernel 1\nnumbers by_spelling\n";
const NATIVE: &str = "function probe.same(x: Any) -> Any\nkernel 1\ncharge 5\nnative\n";
const BODY: &str = "function probe.same(x: Any) -> Any\nkernel 1\ncharge 5\nbody { return x }\n";
const BOTH: &str =
    "function probe.same(x: Any) -> Any\nkernel 1\ncharge 5\nnative\nbody { return x }\n";

struct Same;
impl NativeFunction for Same {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        call.counter.spend(1)?;
        Ok(call.args[0].clone())
    }
}

fn definition(text: &str) -> FunctionDefinition {
    parse_definition(text).expect("parse the rule's definition")
}

fn doc(text: &str) -> Document {
    parse_document(text).expect("parse the rule's document")
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
        "K-DOC-002" | "K-ADM-007" => manifest_closure(),
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
        "K-DOC-007" | "K-ADM-009" => annotations(shard, registry),
        "K-ID-001" | "K-ID-003" => identity_document(shard),
        "K-ID-002" | "K-VER-002" => identity_definition(),
        "K-LIB-001" => definition_parts(),
        "K-LIB-002" | "K-STMT-002" => implementations(),
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
        "K-LIB-006" => native_repeat(),
        "K-LIB-009" => adoption(),
        "K-LIB-011" => native_versions(),
        "K-TEXT-001" | "K-TEXT-002" | "K-TEXT-003" => text_roundtrip(shard),
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
        "K-STMT-003" => statement_locality(),
        "K-CHG-001" => charge_invariance(shard, registry),
        "K-MACH-001" => instance_isolation(shard, registry),
        "K-MACH-004" => delivery_errors(shard, registry),
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
    assert!(matches!(original.implementation, Implementation::Native));
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
        let id = original.identity().unwrap();
        let expression = doc(&format!(
            "{HEADER}use probe.same = @{id}\nmain {{ finish probe.same(7) }}"
        ));
        let result = validate_document(&expression, &catalog);
        if text == BODY {
            assert!(
                matches!(*result.unwrap_err().reason, InvalidReason::CallNotNative { function, .. } if function == id)
            );
        } else {
            result.unwrap();
        }
        let statement = doc(&format!(
            "{HEADER}use probe.same = @{id}\nmain {{ let x = invoke probe.same(7) finish x }}"
        ));
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
    for (statement, native) in [
        ("do perform echo(1) as Int", false),
        ("do sleep 1", true),
        ("do yield", true),
    ] {
        let text = format!(
            "function probe.bad() -> Any\nkernel 1\ncharge 1\n{}body {{ {statement} }}",
            if native { "native\n" } else { "" }
        );
        let invalid = validate_definition(&definition(&text), &catalog).unwrap_err();
        assert!(matches!(
            *invalid.reason,
            InvalidReason::PerformInLibrary { .. } | InvalidReason::NativeBodyMayWait { .. }
        ));
    }
}

fn outer(inner: &FunctionDefinition) -> FunctionDefinition {
    let id = inner.identity().unwrap();
    definition(&format!(
        "function probe.outer(x: Any) -> Any\nkernel 1\ncharge 5\nuse probe.same = @{id}\nbody {{ let y = invoke probe.same(x) return y }}"
    ))
}

fn manifest_closure() {
    let mut registry = same_registry(BODY, false);
    let inner = definition(BODY);
    let outer = outer(&inner);
    let outer_id = registry.register(outer, None).unwrap();
    let inner_id = inner.identity().unwrap();
    let mut document = doc(&format!(
        "{HEADER}use probe.outer = @{outer_id}\nmain {{ let x = invoke probe.outer(7) finish x }}"
    ));
    let needs = requirements(&document, &registry);
    assert_eq!(needs.functions.len(), 2);
    assert!(needs.functions.contains_key(&inner_id));
    let errors = admit(&document, &Environment::new(&registry))
        .unwrap_err()
        .errors;
    assert!(errors.iter().any(|e| matches!(e.reason, RefusalReason::FunctionNotListed { function, .. } if function == inner_id)));
    document.manifest.functions.insert(inner_id, inner.name);
    admit(&document, &Environment::new(&registry)).unwrap();
}

fn missing_transitive() {
    let inner = definition(BODY);
    let outer = outer(&inner);
    let id = outer.identity().unwrap();
    let inner_id = inner.identity().unwrap();
    let catalog = BTreeMap::from([(id, outer)]);
    let document = doc(&format!(
        "{HEADER}use probe.outer = @{id}\nuse probe.same = @{inner_id}\nmain {{ let x = invoke probe.outer(7) finish x }}"
    ));
    let errors = admit(&document, &Environment::new(&catalog))
        .unwrap_err()
        .errors;
    assert!(errors.iter().any(|e| matches!(e.reason, RefusalReason::MissingFunction { function, .. } if function == inner_id)));
}

fn reached_body_rules() {
    for statement in ["break", "return missing"] {
        let bad = definition(&format!(
            "function probe.bad() -> Any\nkernel 1\ncharge 1\nbody {{ {statement} }}"
        ));
        let id = bad.identity().unwrap();
        let catalog = BTreeMap::from([(id, bad)]);
        let document = doc(&format!(
            "{HEADER}use probe.bad = @{id}\nmain {{ let x = invoke probe.bad() finish x }}"
        ));
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
        let mut env = Environment::new(registry);
        env.effects = document.manifest.effects.clone();
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
    let first_id = first.identity().unwrap();
    let next_id = registry.register(corrected.clone(), None).unwrap();
    assert_ne!(first_id, next_id);
    let text = format!(
        "{HEADER}use probe.same = @{first_id}\nmain {{ let x = invoke probe.same(7) finish x }}"
    );
    for (text, result) in [
        (&text, 7),
        (
            &text.replace(&first_id.to_string(), &next_id.to_string()),
            9,
        ),
    ] {
        let mut case = super::case("adopt a corrected library identity");
        case.document = text.clone();
        case.expected.end = ExpectedEnd::Finished(lash_kernel_doc::Datum::Int(result.into()));
        let mut runner = MachineRunner::<KernelMachine>::new(Arc::new(registry.clone()));
        check_case(&mut runner, &case).unwrap();
    }
}

fn native_versions() {
    let first = definition(NATIVE);
    assert_eq!(first.native_version, 1);
    let second = definition(&NATIVE.replace("native\n", "native 2\n"));
    assert_eq!(second.native_version, 2);
    assert_ne!(first.identity().unwrap(), second.identity().unwrap());
    assert_eq!(definition(&print_definition(&second)), second);
    let mut zero = first;
    zero.native_version = 0;
    let empty = FunctionRegistry::new();
    assert!(matches!(
        *validate_definition(&zero, &empty).unwrap_err().reason,
        InvalidReason::NativeVersionZero
    ));
    let mut body = definition(BODY);
    body.native_version = 2;
    assert!(matches!(
        *validate_definition(&body, &empty).unwrap_err().reason,
        InvalidReason::NativeVersionWithoutNative { version: 2 }
    ));
}

fn native_repeat() {
    let registry = Arc::new(same_registry(NATIVE, true));
    let id = definition(NATIVE).identity().unwrap();
    let mut case = super::case("equal native arguments cold and warm");
    case.document = format!("{HEADER}use probe.same = @{id}\nmain {{ finish probe.same(7) }}");
    case.expected.end = ExpectedEnd::Finished(lash_kernel_doc::Datum::Int(7.into()));
    let mut warm = MachineRunner::<KernelMachine>::new(registry);
    let first = check_case(&mut warm, &case).unwrap();
    assert_eq!(check_case(&mut warm, &case).unwrap(), first);
    let mut cold = MachineRunner::<KernelMachine>::new(Arc::new(same_registry(NATIVE, true)));
    assert_eq!(check_case(&mut cold, &case).unwrap(), first);
}

fn annotations(shard: &Shard, registry: &FunctionRegistry) {
    let document = doc(&shard.cases[0].document);
    let id = document.identity().unwrap();
    let graph = derive(&document, registry).unwrap();
    let mut layer = Annotations::new(id);
    layer.source = Some("arbitrary authored source".into());
    layer.dialect = Some("arbitrary dialect".into());
    layer.nodes.push(NodeAnnotation {
        site: Site::new(Unit::Main, [0]),
        label: Some(Label {
            title: "a label".into(),
            description: None,
        }),
        data: BTreeMap::from([("layout".into(), serde_json::json!({"x": 9}))]),
    });
    validate_annotations(&layer, &document).unwrap();
    let stored = Document::from_json(&document.to_json().unwrap()).unwrap();
    assert_eq!(stored.identity().unwrap(), id);
    assert_eq!(derive(&stored, registry).unwrap(), graph);
    assert!(!print_document(&document).contains("arbitrary"));
    drop(layer);
    assert_eq!(derive(&document, registry).unwrap(), graph);
}

fn stored_encoding(shard: &Shard) {
    let document = doc(&shard.cases[0].document);
    let json = document.to_json().unwrap();
    assert_eq!(Document::from_json(&json).unwrap(), document);
    let mut tree: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(tree.get("functions").is_none());
    assert!(tree["manifest"].get("effects").is_none());
    assert_eq!(
        tree["main"][0]["finish"]["value"]["tuple"][0],
        serde_json::json!({"literal": {"int": "1"}})
    );
    for pointer in [
        "",
        "/manifest",
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
        for (index, _) in node.children().iter().enumerate() {
            let mut child = site.clone();
            child.path.push(index as u32);
            walk(document, child, result);
        }
    }
    let document = doc(&shard.cases[0].document);
    let graph = derive(&document, registry).unwrap();
    let mut sites = Vec::new();
    walk(&document, Site::new(Unit::Main, []), &mut sites);
    for name in document.functions.keys() {
        walk(
            &document,
            Site::new(Unit::Function(name.clone()), []),
            &mut sites,
        );
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
    let id = body.identity().unwrap();
    let library = derive_definition(&body, registry).unwrap();
    assert!(
        library
            .nodes()
            .iter()
            .all(|node| node.site.unit == Unit::Library(id))
    );
}

fn statement_locality() {
    let registry = same_registry(BODY, false);
    let id = definition(BODY).identity().unwrap();
    for body in [
        "let x = 7 let y = invoke probe.same(x) finish y",
        "let x = 7 print 0 let y = invoke probe.same(x) print 0 finish y",
    ] {
        let document = doc(&format!(
            "{HEADER}use probe.same = @{id}\nmain {{ {body} }}"
        ));
        admit(&document, &Environment::new(&registry)).unwrap();
    }
}

fn charge_invariance(shard: &Shard, registry: &FunctionRegistry) {
    let mut case = shard.cases[0].clone();
    let mut runner = MachineRunner::<KernelMachine>::new(Arc::new(registry.clone()));
    let resumed = check_case(&mut runner, &case).unwrap();
    case.environment.resume = false;
    assert_eq!(check_case(&mut runner, &case).unwrap(), resumed);
    let id = definition(BOTH).identity().unwrap();
    let document = doc(&format!(
        "{HEADER}use probe.same = @{id}\nmain {{ let x = invoke probe.same(7) finish x }}"
    ));
    let mut observations = Vec::new();
    for native in [false, true] {
        for layout in [Layout(0), Layout(37)] {
            let program = Program {
                document: Arc::new(document.clone()),
                library: PreparedLibrary::new(Arc::new(same_registry(BOTH, native))),
            };
            let mut machine = KernelMachine::start_with_layout(
                program,
                crate::CaseBounds::default().into(),
                start(),
                layout,
            )
            .unwrap();
            let mut host = ProbeHost;
            let end = machine.run(&mut host, u64::MAX).unwrap();
            observations.push((end, machine.meters().charged));
        }
    }
    assert!(observations.iter().all(|item| item == &observations[0]));
}

fn instance_isolation(shard: &Shard, registry: &FunctionRegistry) {
    let program = program(&shard.cases[0], registry);
    let mut first = KernelMachine::start(
        program.clone(),
        crate::CaseBounds::default().into(),
        start(),
    )
    .unwrap();
    let mut second =
        KernelMachine::start(program, crate::CaseBounds::default().into(), start()).unwrap();
    let mut host = ProbeHost;
    assert_eq!(first.run(&mut host, 1).unwrap(), Step::Slice);
    let second_end = second.run(&mut host, u64::MAX).unwrap();
    let first_end = first.run(&mut host, u64::MAX).unwrap();
    assert_eq!(first_end, second_end);
    assert_eq!(first.meters().charged, second.meters().charged);
    assert_eq!(first.meters().memory, second.meters().memory);
}

fn program(case: &Case, registry: &FunctionRegistry) -> Program {
    Program {
        document: Arc::new(doc(&case.document)),
        library: PreparedLibrary::new(Arc::new(registry.clone())),
    }
}

fn delivery_errors(shard: &Shard, registry: &FunctionRegistry) {
    let mut machine = KernelMachine::start(
        program(&shard.cases[0], registry),
        crate::CaseBounds::default().into(),
        start(),
    )
    .unwrap();
    let mut host = ProbeHost;
    let Step::Parked(park) = machine.run(&mut host, u64::MAX).unwrap() else {
        panic!("effect parks")
    };
    let Request::Effect(request) = &park.requests[0] else {
        panic!("effect request")
    };
    let before = machine.export().unwrap();
    assert!(matches!(
        machine.deliver(
            WaitId(u64::MAX),
            Outcome::Completed(lash_kernel_doc::Datum::Null)
        ),
        Err(DeliverError::UnknownWait { .. })
    ));
    assert_eq!(machine.export().unwrap(), before);
    assert_eq!(
        machine
            .deliver(
                request.wait,
                Outcome::Completed(lash_kernel_doc::Datum::Int(2.into()))
            )
            .unwrap(),
        Delivered::Accepted
    );
    let accepted = machine.export().unwrap();
    assert!(matches!(
        machine.deliver(
            request.wait,
            Outcome::Completed(lash_kernel_doc::Datum::Int(99.into()))
        ),
        Err(DeliverError::AlreadyDelivered { .. })
    ));
    assert_eq!(machine.export().unwrap(), accepted);
    assert!(matches!(
        machine.run(&mut host, u64::MAX).unwrap(),
        Step::Ended(End::Finished(_))
    ));
}

struct ProbeHost;
impl lash_kernel_vm::Host for ProbeHost {
    fn clock(&mut self) -> lash_kernel_doc::Timestamp {
        panic!("no clock in this case")
    }
    fn random(&mut self) -> u64 {
        panic!("no random in this case")
    }
    fn read(
        &mut self,
        _: &lash_kernel_doc::Handle,
        _: &lash_kernel_doc::Datum,
    ) -> Result<lash_kernel_doc::Datum, lash_kernel_doc::ErrorDatum> {
        panic!("no projection read in this case")
    }
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
        "327c6a61cbff309611ad4af70ad4f9cb4ba2e9bc515bbd3458f8f5dea83a4e7f"
    } else {
        "aaaced107a063053435c79613097364cfe8afdda36f476681a9e0f912b7e2b3b"
    };
    assert_eq!(document.identity().unwrap().to_string(), expected);
    let different = doc(&format!("{HEADER}main {{ finish 8 }}"));
    assert_ne!(document.identity().unwrap(), different.identity().unwrap());
}

fn identity_definition() {
    let original = definition(NATIVE);
    assert_eq!(
        original.identity().unwrap().to_string(),
        "72975a48e74d563086004e9d5e17c6664336158d7b9b75717b84d07a9f1f2d4f"
    );
    let first = original.identity().unwrap();
    let json: serde_json::Value = serde_json::from_str(&original.to_json().unwrap()).unwrap();
    for (field, value) in [
        ("name", serde_json::json!("probe.other")),
        (
            "signature",
            serde_json::json!({"params": [{"name": "x", "ty": "int"}], "result": "any"}),
        ),
        ("errors", serde_json::json!(["probe.error"])),
        ("charge", serde_json::json!({"constant": 6})),
        (
            "guard",
            serde_json::json!({"unit": "step", "limit": {"constant": 9}}),
        ),
        ("implementation", serde_json::json!({"body": {"block": []}})),
    ] {
        let mut changed = json.clone();
        changed[field] = value;
        let changed = FunctionDefinition::from_json(&changed.to_string()).unwrap();
        assert_ne!(changed.identity().unwrap(), first, "{field}");
        assert_eq!(changed.kernel, 1);
    }
}

fn start() -> Start {
    Start {
        target: lash_kernel_vm::Target::Main,
        args: Vec::new(),
        bindings: Default::default(),
    }
}
