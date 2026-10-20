//! CONF-NATIVE, CONF-SHAPE and CONF-METER over installed packages.
#![allow(clippy::unwrap_used, reason = "a malformed law fixture fails its law")]

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_ext_helper_compiler::library::{Dialect, Library};
use lash_kernel_doc::{
    Datum, FunctionRegistry, Implementation, NativeCall, NativeError, NativeFunction, Type, Value,
};
use lash_kernel_package::{HeldPackages, Package, PackageSources};

use super::{Kit, Law, Witness};
use crate::{Case, Expected, ExpectedEnd};

fn case(name: &str, document: String) -> Case {
    Case {
        name: name.into(),
        document,
        environment: Default::default(),
        expected: Expected {
            prints: vec![],
            end: ExpectedEnd::Finished(Datum::Null),
            trace: vec![],
            charged: None,
            parks: None,
        },
    }
}

fn close(mut case: Case, registry: &FunctionRegistry) -> Case {
    let mut document = lash_kernel_doc::parse_document(&case.document).unwrap();
    let required = lash_kernel_check::requirements(&document, registry);
    document.manifest.functions = required.functions;
    for (effect, bodies) in required.performs {
        document
            .manifest
            .effects
            .insert(effect, bodies.into_values().next().unwrap());
    }
    case.document = lash_kernel_doc::print_document(&document);
    case
}

#[path = "fixtures.rs"]
mod fixtures;
use fixtures::installed;

fn sample(ty: &Type, variant: usize) -> String {
    match ty {
        Type::Any => [
            "null",
            "absent",
            "false",
            "0.0",
            "-1.0",
            "\"x\"",
            "[1, 2]",
            "{}",
            "{__lash_brand: (\"test\", \"value\")}",
        ][variant]
            .into(),
        Type::Bool => if variant.is_multiple_of(2) {
            "false"
        } else {
            "true"
        }
        .into(),
        Type::Int => "0".into(),
        Type::Float | Type::Number => "0.0".into(),
        Type::Text => if variant == 0 { "\"\"" } else { "\"x\"" }.into(),
        Type::Bytes => "b\"\"".into(),
        Type::Absent => "absent".into(),
        Type::Tuple(items) => format!(
            "({}{})",
            items
                .iter()
                .map(|ty| sample(ty, variant))
                .collect::<Vec<_>>()
                .join(", "),
            if items.len() == 1 { "," } else { "" }
        ),
        Type::List(item) => {
            if variant == 0 {
                "[]".into()
            } else {
                format!("[{}]", sample(item, variant))
            }
        }
        Type::Map(_) => "map{}".into(),
        Type::Set(_) => "set{}".into(),
        Type::Record(record) => format!(
            "{{{}}}",
            record
                .fields
                .iter()
                .filter(|f| !f.optional)
                .map(|f| format!("{}: {}", f.name, sample(&f.ty, variant)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Type::Enum(items) => serde_json::to_string(&items[0]).unwrap(),
        Type::Union(items) => sample(&items[0], variant),
        _ => "null".into(),
    }
}

/// Every installed Both function gets a typed empty-input witness; native-only
/// definitions are counted explicitly because they have no body to compare.
#[test]
fn conf_native_six_packages_and_dataframe() {
    let (held, registry) = installed();
    let view = held.view(&held.own()).unwrap();
    let mut owners = BTreeSet::new();
    let mut both = 0;
    let mut native_only = 0;
    let registry = Arc::new(registry);
    let kit = Kit::new(view, registry.clone());
    let mut witnesses = vec![];
    for package in held.view(&held.own()).unwrap().packages() {
        owners.insert(package.path().to_string());
        for id in package.ids() {
            let function = registry.get(&id).unwrap();
            let definition = function.definition();
            match &definition.implementation {
                Implementation::Native => {
                    native_only += 1;
                    continue;
                }
                Implementation::Body(_) => continue,
                Implementation::Both(_) => {}
            }
            if function.native().is_none() {
                // Both permits an optional native; Python currently installs plans.
                assert!(
                    function.plan().is_some(),
                    "{} lacks an implementation",
                    function.name()
                );
            }
            for variant in 0..9 {
                let args = definition
                    .signature
                    .params
                    .iter()
                    .map(|p| sample(&p.ty, variant))
                    .collect::<Vec<_>>()
                    .join(", ");
                let document = format!(
                    "kernel 1\nnumbers by_spelling\nuse {} = @{id}\nmain {{ let answer = invoke {}({args}) finish answer }}",
                    function.name(),
                    function.name()
                );
                let witness = Witness {
                    function: id,
                    law: Law::Native,
                    case: close(case(function.name(), document), &registry),
                };
                witnesses.push(witness);
            }
            both += 1;
        }
    }
    assert_eq!(
        owners,
        [
            "kernel",
            "regex.ecma",
            "date.ecma",
            "url.whatwg",
            "typescript",
            "python",
            "dataframe"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    let checked = kit.check_all(&witnesses).unwrap();
    println!(
        "CONF-NATIVE: packages={} Both={both} native-only={native_only} witnesses={checked}",
        owners.len()
    );
}

struct Wrong;
impl NativeFunction for Wrong {
    fn call(&self, _: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Ok(Value::Null)
    }
}

fn planted(kernel: &Package, shape: bool) -> Package {
    let (kernel_source, surface) = if shape {
        (
            "function planted.name(self: item) -> Text\nkernel 1\ncharge 2\nnative\nbody { return self.name }",
            "type item = Record{name: Text}\n  property name(self: item) -> Text = planted.name",
        )
    } else {
        (
            "function planted.answer() -> Int\nkernel 1\ncharge 2\nnative\nbody { return 42 }",
            "function answer() -> Int = planted.answer",
        )
    };
    Package::build(
        "planted",
        "1.0",
        &PackageSources {
            kernel: kernel_source,
            surface,
            labels: "",
        },
        &[kernel],
    )
    .unwrap()
}

/// The kit rejects a wrong native, naming the value difference.
#[test]
fn conf_native_wrong_native_names_the_difference() {
    let base = Library::of(Dialect::Python).unwrap();
    let kernel = base.packages[0].clone();
    let package = planted(&kernel, false);
    let id = package.function("planted.answer").unwrap();
    let held = HeldPackages::install([kernel, package]).unwrap();
    let mut registry = FunctionRegistry::declarations();
    held.register_into(&mut registry, &|function| {
        if *function == id {
            Some(Arc::new(Wrong))
        } else {
            base.registry
                .get(function)
                .and_then(|f| f.native().cloned())
        }
    })
    .unwrap();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let witness = Witness {
        function: id,
        law: Law::Native,
        case: close(
            case(
                "wrong native",
                format!(
                    "kernel 1\nnumbers by_spelling\nuse planted.answer = @{id}\nmain {{ let answer = invoke planted.answer() finish answer }}"
                ),
            ),
            &registry,
        ),
    };
    let error = kit.check(&witness).unwrap_err().to_string();
    assert!(
        error.contains("CONF-NATIVE planted planted.answer")
            && error.contains("value or error differs"),
        "{error}"
    );
}

/// The kit rejects a native that returns without its generated shape check.
#[test]
fn conf_shape_skipped_check_names_the_difference() {
    let base = Library::of(Dialect::Python).unwrap();
    let kernel = base.packages[0].clone();
    let package = planted(&kernel, true);
    let id = package.function("planted.name").unwrap();
    let held = HeldPackages::install([kernel, package]).unwrap();
    let mut registry = FunctionRegistry::declarations();
    held.register_into(&mut registry, &|function| {
        if *function == id {
            Some(Arc::new(Wrong))
        } else {
            base.registry
                .get(function)
                .and_then(|f| f.native().cloned())
        }
    })
    .unwrap();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let witness = Witness {
        function: id,
        law: Law::Shape,
        case: close(
            case(
                "missing name",
                format!(
                    "kernel 1\nnumbers by_spelling\nuse planted.name = @{id}\nmain {{ let answer = invoke planted.name({{__lash_brand: (\"planted\", \"item\"), __lash_built: b\"00\"}}) finish answer }}"
                ),
            ),
            &registry,
        ),
    };
    let error = kit.check(&witness).unwrap_err().to_string();
    assert!(
        error.contains("CONF-SHAPE planted planted.name")
            && error.contains("value or error differs"),
        "{error}"
    );
    let brand = |package: &str| {
        Datum::Tuple(vec![
            Datum::Text(package.into()),
            Datum::Text("item".into()),
        ])
    };
    for (input, expected_kind) in [
        (Datum::Null, "type_error"),
        (
            Datum::Record(vec![("__lash_brand".into(), brand("other"))]),
            "type_error",
        ),
        (
            Datum::Record(vec![
                ("name".into(), Datum::Int(3.into())),
                ("__lash_brand".into(), brand("planted")),
                (
                    "__lash_built".into(),
                    Datum::Bytes(lash_kernel_doc::Bytes::new(vec![0])),
                ),
            ]),
            "package.stale_value",
        ),
    ] {
        let mut case = close(
            case(
                "shape input",
                format!(
                    "kernel 1\nnumbers by_spelling\nuse planted.name = @{id}\nentry probe(value: Any) -> Any\nfn probe(value) {{ let answer = invoke planted.name(value) return answer }}\nmain {{ finish null }}"
                ),
            ),
            &registry,
        );
        case.environment.entry = Some(lash_kernel_doc::Name::new("probe"));
        case.environment.args = vec![input];
        let (body, _) = super::observe(
            &kit.bodies,
            &case,
            lash_kernel_vm::kernel_internals::Layout(3),
        )
        .unwrap();
        assert!(
            matches!(&body.end, ExpectedEnd::Uncaught(Datum::Error(error)) if error.kind == expected_kind),
            "{body:?}"
        );
        let error = kit
            .check(&Witness {
                function: id,
                law: Law::Shape,
                case,
            })
            .unwrap_err();
        assert!(error.0.contains("value or error differs"), "{error}");
    }
    let good = close(
        case(
            "valid shape",
            format!(
                "kernel 1\nnumbers by_spelling\nuse planted.name = @{id}\nmain {{ let answer = invoke planted.name({{name: \"good\", __lash_brand: (\"planted\", \"item\"), __lash_built: b\"00\"}}) finish answer }}"
            ),
        ),
        &registry,
    );
    let (body, _) = super::observe(
        &kit.bodies,
        &good,
        lash_kernel_vm::kernel_internals::Layout(3),
    )
    .unwrap();
    assert_eq!(body.end, ExpectedEnd::Finished(Datum::Text("good".into())));
}

/// A host-controlled polling loop is not bounded by any argument or result.
#[test]
fn conf_meter_unbounded_performing_loop_names_the_difference() {
    let base = Library::of(Dialect::Python).unwrap();
    let kernel = base.packages[0].clone();
    let package = Package::build("planted", "1.0", &PackageSources {
        kernel: "effect planted.poll() -> Bool contract \"planted.poll/1\"\nfunction planted.polling() -> Null\nkernel 1\ncharge 2\nbody { let again = true while again { set again = perform planted.poll() as Bool } return null }",
        surface: "operation poll() -> Bool contract \"planted.poll/1\"\nasync function polling() -> Null = planted.polling",
        labels: "",
    }, &[&kernel]).unwrap();
    let held = HeldPackages::install([kernel, package]).unwrap();
    let error = super::audit_metering(&held.view(&held.own()).unwrap(), &super::LoopBounds::new())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("CONF-METER planted planted.polling") && error.contains("declare metered N"),
        "{error}"
    );
}

/// A metered package loop whose guest callback performs: plans and bodies
/// retain the same continuation events, requests, results and charge.
#[test]
fn conf_meter_performing_callback_plan_emits_the_bodys_events() {
    let (held, registry) = installed();
    let id = registry.named("ts.array.forEach").next().unwrap();
    let document = format!(
        "kernel 1\nnumbers by_spelling\neffect echo(value: Any) -> Null contract \"1\"\nuse ts.array.forEach = @{id}\nmain {{ let visit = fn(context, args) {{ let value = args[0] do perform echo(value) as Null return null }} let callbacks = [visit] let xs = [1, 2] do invoke ts.array.forEach(xs, callbacks) finish null }}"
    );
    let mut case = close(case("metered callback performs twice", document), &registry);
    case.environment.deliveries = (0..2)
        .map(|request| {
            vec![crate::Delivery {
                request,
                outcome: crate::ScriptOutcome::Completed(Datum::Null),
                dropped: false,
            }]
        })
        .collect();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry);
    let (reference, tape) = super::observe(
        &kit.bodies,
        &case,
        lash_kernel_vm::kernel_internals::Layout(3),
    )
    .unwrap();
    assert_eq!(reference.end, ExpectedEnd::Finished(Datum::Null));
    assert_eq!(reference.trace.len(), 2);
    assert_eq!(reference.parks, 2);
    assert!(tape.contains(&lash_kernel_exec::charge::Weight::Atomic(2)));
    kit.check(&Witness {
        function: id,
        law: Law::Meter,
        case,
    })
    .unwrap();
}

/// CONF-SHAPE: a Both prologue checks nested package types with the same
/// path and artifact data as a body-only member, then runs valid input.
#[test]
fn conf_shape_both_members_check_nested_values_with_their_own_artifacts() {
    let base = Library::of(Dialect::Python).unwrap();
    let kernel = base.packages[0].clone();
    let package = Package::build("nested", "1.0", &PackageSources {
        kernel: "function nested.name(self: bundle, children: List(item)) -> Text\nkernel 1\ncharge 2\nnative\nbody { return self.item.name }",
        surface: "type item = Record{name: Text}\ntype bundle = Record{item: item}\n  method name(self: bundle, children: List(item)) -> Text = nested.name",
        labels: "",
    }, &[&kernel]).unwrap();
    let id = package.function("nested.name").unwrap();
    let held = HeldPackages::install([kernel, package]).unwrap();
    let mut registry = FunctionRegistry::declarations();
    held.register_into(&mut registry, &|function| {
        base.registry
            .get(function)
            .and_then(|f| f.native().cloned())
    })
    .unwrap();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let good = "{name: \"good\", __lash_brand: (\"nested\", \"item\"), __lash_built: b\"01\"}";
    let stale = "{name: 3, __lash_brand: (\"nested\", \"item\"), __lash_built: b\"01\"}";
    for (item, children, expected_path) in [
        (stale, format!("[{good}]"), "self.item.name"),
        (good, format!("[{stale}]"), "children[].name"),
    ] {
        let case = close(
            case(
                "nested shape",
                format!(
                    "kernel 1\nnumbers by_spelling\nuse nested.name = @{id}\nmain {{ let bundle = {{item: {item}, __lash_brand: (\"nested\", \"bundle\"), __lash_built: b\"02\"}} let children = {children} let name = invoke nested.name(bundle, children) finish name }}"
                ),
            ),
            &registry,
        );
        let (body, _) = super::observe(
            &kit.bodies,
            &case,
            lash_kernel_vm::kernel_internals::Layout(3),
        )
        .unwrap();
        let ExpectedEnd::Uncaught(Datum::Error(error)) = body.end else {
            panic!("{body:?}");
        };
        let stale = lash_kernel_package::StaleValue::of(&error).unwrap();
        assert_eq!(stale.at, expected_path);
        assert_eq!(stale.current, id, "a Both prologue names its own member");
        let Datum::Record(fields) = error.data else {
            panic!("stale data is a record")
        };
        assert!(fields.contains(&(
            "built".into(),
            Datum::Bytes(lash_kernel_doc::Bytes::new(vec![1]))
        )));
        kit.check(&Witness {
            function: id,
            law: Law::Shape,
            case,
        })
        .unwrap();
    }
    let case = close(
        case(
            "valid nested shape",
            format!(
                "kernel 1\nnumbers by_spelling\nuse nested.name = @{id}\nmain {{ let bundle = {{item: {good}, __lash_brand: (\"nested\", \"bundle\"), __lash_built: b\"02\"}} let children = [{good}] let name = invoke nested.name(bundle, children) finish name }}"
            ),
        ),
        &registry,
    );
    let (body, _) = super::observe(
        &kit.bodies,
        &case,
        lash_kernel_vm::kernel_internals::Layout(3),
    )
    .unwrap();
    assert_eq!(body.end, ExpectedEnd::Finished(Datum::Text("good".into())));
}

/// Runs `main` with `uses` under the reference bodies and the installed
/// code, which must agree in every observation and charge event. Returns
/// the reference observations and how many two-unit events it charged.
fn metered_events(
    kit: &Kit<'_>,
    registry: &FunctionRegistry,
    uses: &[&str],
    main: &str,
) -> (crate::Observations, usize) {
    let uses = uses
        .iter()
        .map(|name| format!("use {name} = @{}\n", registry.named(name).next().unwrap()))
        .collect::<String>();
    let case = close(
        case(main, format!("kernel 1\nnumbers by_spelling\n{uses}{main}")),
        registry,
    );
    let (body, events) = super::observe(
        &kit.bodies,
        &case,
        lash_kernel_vm::kernel_internals::Layout(3),
    )
    .unwrap();
    let (installed, installed_events) = super::observe(
        &kit.installed,
        &case,
        lash_kernel_vm::kernel_internals::Layout(0),
    )
    .unwrap();
    assert_eq!(installed, body, "{main}");
    assert_eq!(installed_events, events, "{main}");
    let metered = events
        .iter()
        .filter(|event| **event == lash_kernel_exec::charge::Weight::Atomic(2))
        .count();
    (body, metered)
}

/// How many more continuation tests the library charged for `n = 64`
/// visits than for `n = 8`. Each size runs beside a control that makes the
/// same guest calls without the library, so guest events cancel out.
fn metered_slope(
    kit: &Kit<'_>,
    registry: &FunctionRegistry,
    uses: &[&str],
    main: impl Fn(u32, bool) -> String,
) -> i64 {
    let library = |n| {
        let (_, helper) = metered_events(kit, registry, uses, &main(n, false));
        let (_, control) = metered_events(kit, registry, uses, &main(n, true));
        i64::try_from(helper).unwrap() - i64::try_from(control).unwrap()
    };
    library(64) - library(8)
}

/// A library traversal over a live input that applies guest code, with the
/// adversarial guest code for it: each visit appends one element until the
/// `n`th, which clears the input and then returns the value that keeps the
/// traversal going, or raises where the helper's output would bound it.
struct Traversal {
    helper: &'static str,
    params: &'static str,
    setup: &'static str,
    items: &'static str,
    element: &'static str,
    call: &'static str,
    control: &'static str,
    keep: &'static str,
    raises: bool,
}

const fn applying(
    helper: &'static str,
    call: &'static str,
    keep: &'static str,
    raises: bool,
) -> Traversal {
    Traversal {
        helper,
        params: "x",
        setup: "set xs[0] = 0",
        items: "xs",
        element: "n",
        call,
        control: "for x in xs { do apply f(x) }",
        keep,
        raises,
    }
}

/// Live aliases: the callback reaches the source it walks. The last entry
/// reaches it through a coercion (`valueOf`), not a direct callback, and
/// its nested coercion helpers add metered tests of their own per visit.
const TRAVERSALS: &[Traversal] = &[
    applying(
        "collection.all",
        "invoke collection.all(xs, f)",
        "true",
        false,
    ),
    applying(
        "collection.any",
        "invoke collection.any(xs, f)",
        "false",
        false,
    ),
    applying(
        "collection.filter",
        "invoke collection.filter(xs, f)",
        "false",
        false,
    ),
    applying(
        "collection.find",
        "invoke collection.find(xs, f)",
        "false",
        false,
    ),
    applying(
        "collection.for_each",
        "invoke collection.for_each(xs, f)",
        "null",
        false,
    ),
    Traversal {
        params: "acc, x",
        control: "let acc = 0 for x in xs { set acc = apply f(acc, x) }",
        ..applying(
            "collection.reduce",
            "invoke collection.reduce(xs, f, 0)",
            "0",
            false,
        )
    },
    applying("collection.map", "invoke collection.map(xs, f)", "x", true),
    applying(
        "collection.group_by",
        "invoke collection.group_by(xs, f)",
        "0",
        true,
    ),
    Traversal {
        params: "context, args",
        setup: "set xs = {pairs: [(\"a\", \"b\")]}",
        items: "xs.pairs",
        element: "(n, n)",
        control: "for p in xs.pairs { do apply f(null, [p[1], p[0], xs]) }",
        ..applying(
            "ts.url_search_params.forEach",
            "invoke ts.url_search_params.forEach(xs, [f])",
            "null",
            false,
        )
    },
    Traversal {
        params: "context, args",
        control: "let a = [0, 0.0] for x in xs { do apply f(absent, a) }",
        ..applying(
            "ts.map.groupBy",
            "invoke ts.map.groupBy(null, [xs, f])",
            "0.0",
            true,
        )
    },
    Traversal {
        params: "this, args",
        setup: "set o = {valueOf: f} set xs[0] = o",
        element: "o",
        control: "for a in xs { do apply f(a, []) }",
        ..applying("ts.math.max", "invoke ts.math.max(null, xs)", "1.0", false)
    },
];

impl Traversal {
    const USES: [&str; 3] = ["num.add", "num.lt", "list.len"];

    /// The witness for `n` visits, finishing with the visit count and the
    /// input's final length whether or not it raised. The control applies
    /// the callback from a guest loop instead of the helper.
    fn main(&self, n: u32, control: bool) -> String {
        let Self {
            params,
            setup,
            items,
            element,
            call,
            keep,
            ..
        } = self;
        let last = if self.raises {
            "throw \"discarded\""
        } else {
            ""
        };
        let run = if control {
            self.control.to_owned()
        } else {
            format!("do {call}")
        };
        format!(
            "main {{ let xs = [] let o = absent let n = 0 \
             let f = fn({params}) {{ let items = {items} set n = num.add(n, 1) print n \
             if num.lt(n, {n}) {{ set items[list.len(items)] = {element} }} \
             else {{ while num.lt(0, list.len(items)) {{ remove items[0] }} {last} }} return {keep} }} \
             {setup} try {{ {run} }} catch e {{ }} finish (n, list.len({items})) }}"
        )
    }

    fn uses(&self) -> Vec<&str> {
        let mut uses = Self::USES.to_vec();
        uses.push(self.helper);
        uses
    }
}

/// K-CHG-007, live alias: guest code that grows the walked input and
/// clears it before completion cannot buy uncharged library work. Each
/// traversal bills two units for every continuation test, under its body
/// and its installed code alike, so its charge follows the visits, not
/// the final size.
#[test]
fn conf_meter_live_alias_traversals_charge_every_visit_of_a_grown_and_cleared_input() {
    let (held, registry) = installed();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let mut failures = vec![];
    for traversal in TRAVERSALS {
        let uses = traversal.uses();
        for n in [8, 64] {
            let (body, _) = metered_events(&kit, &registry, &uses, &traversal.main(n, false));
            assert_eq!(
                body.end,
                ExpectedEnd::Finished(Datum::Tuple(vec![
                    Datum::Int(i64::from(n).into()),
                    Datum::Int(0.into()),
                ])),
                "{}",
                traversal.helper
            );
        }
        let slope = metered_slope(&kit, &registry, &uses, |n, control| {
            traversal.main(n, control)
        });
        if slope < 56 {
            failures.push(format!(
                "{}: {slope} more library tests for 56 more visits",
                traversal.helper
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A charge bound reached mid-traversal stops the installed code at the
/// reference body's visit, with its charge and its ordered charge events.
#[test]
fn conf_meter_bounded_traversals_stop_at_the_reference_visit() {
    let (held, registry) = installed();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    for traversal in TRAVERSALS {
        let uses = traversal
            .uses()
            .iter()
            .map(|name| format!("use {name} = @{}\n", registry.named(name).next().unwrap()))
            .collect::<String>();
        let mut case = close(
            case(
                traversal.helper,
                format!(
                    "kernel 1\nnumbers by_spelling\n{uses}{}",
                    traversal.main(64, false)
                ),
            ),
            &registry,
        );
        let (whole, _) = super::observe(
            &kit.bodies,
            &case,
            lash_kernel_vm::kernel_internals::Layout(3),
        )
        .unwrap();
        case.environment.bounds.charge = whole.charged.unwrap() / 2;
        let (reference, events) = super::observe(
            &kit.bodies,
            &case,
            lash_kernel_vm::kernel_internals::Layout(3),
        )
        .unwrap();
        assert!(
            matches!(reference.end, ExpectedEnd::Bound { .. })
                && (1..63).contains(&reference.prints.len()),
            "{}: {reference:?}",
            traversal.helper
        );
        let (installed, installed_events) = super::observe(
            &kit.installed,
            &case,
            lash_kernel_vm::kernel_internals::Layout(0),
        )
        .unwrap();
        assert_eq!(installed, reference, "{}", traversal.helper);
        assert_eq!(installed_events, events, "{}", traversal.helper);
    }
}

/// K-CHG-007, coerced-text trip count: a receiver whose `toString` makes an
/// `n`-unit text from nothing the call holds still pays for every position
/// the search visits, though the receiver, arguments and result stay tiny.
#[test]
fn conf_meter_coerced_text_trip_counts_charge_every_test() {
    let (held, registry) = installed();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let slope = metered_slope(
        &kit,
        &registry,
        &["ts.string.lastIndexOf", "num.lt", "text.len", "text.concat"],
        |n, control| {
            let run = if control {
                "let r = apply f(o, [])"
            } else {
                "let r = invoke ts.string.lastIndexOf(o, [\"\"])"
            };
            format!(
                "main {{ let f = fn(this, args) {{ let s = \"\" while num.lt(text.len(s), {n}) {{ set s = text.concat(s, \"a\") }} return s }} \
                 let o = {{toString: f}} {run} finish null }}"
            )
        },
    );
    assert!(
        slope >= 56,
        "{slope} more library tests for 56 more positions"
    );
}

/// K-CHG-007, task join: a wait whose later round raises discards the
/// results that bounded its earlier round, so each round's tests are
/// metered as they run.
#[test]
fn conf_meter_task_joins_charge_every_collected_result() {
    let (held, registry) = installed();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let slope = metered_slope(
        &kit,
        &registry,
        &["tasks.wait_all", "num.lt", "num.add"],
        |n, control| {
            let run = if control {
                format!(
                    "set hs[{n}] = s try {{ let vs = join all hs let b = vs[{n}] let x = join b }} catch e {{ }}"
                )
            } else {
                "try { let out = invoke tasks.wait_all() } catch e { }".to_owned()
            };
            format!(
                "fn one() {{ do yield return 1 }}\nfn bad() {{ do yield throw \"bad\" }}\nfn spawner() {{ do yield let b = spawn call bad() return b }}\n\
                 main {{ let hs = [] let i = 0 while num.lt(i, {n}) {{ set hs[i] = spawn call one() set i = num.add(i, 1) }} \
                 let s = spawn call spawner() {run} finish i }}"
            )
        },
    );
    assert_eq!(slope, 56, "more library tests for 56 more results");
}

/// Runs `main` under the reference bodies and the installed code on a
/// thread, failing the law if either does not stop within a minute. The
/// two must agree in every observation and charge event.
fn stopping(
    kit: &Kit<'_>,
    registry: &Arc<FunctionRegistry>,
    helper: &str,
    main: &str,
    charge: u64,
) -> crate::Observations {
    let id = registry.named(helper).next().unwrap();
    let mut case = close(
        case(
            main,
            format!("kernel 1\nnumbers by_spelling\nuse {helper} = @{id}\n{main}"),
        ),
        registry,
    );
    case.environment.bounds.charge = charge;
    let (sender, receiver) = std::sync::mpsc::channel();
    let libraries = [(kit.bodies.clone(), 3), (kit.installed.clone(), 0)];
    let run = case.clone();
    std::thread::spawn(move || {
        let observed = libraries.map(|(library, layout)| {
            super::observe(
                &library,
                &run,
                lash_kernel_vm::kernel_internals::Layout(layout),
            )
            .unwrap()
        });
        sender.send(observed).ok();
    });
    let [(body, events), (installed, installed_events)] = receiver
        .recv_timeout(std::time::Duration::from_secs(60))
        .unwrap_or_else(|_| panic!("{main} did not stop"));
    assert_eq!(installed, body, "{main}");
    assert_eq!(installed_events, events, "{main}");
    body
}

/// A non-finite number has no exact ratio: NaN never equals its floor, so
/// the doubling loop would never end. The helper raises at entry instead.
#[test]
fn conf_meter_ratio_of_a_non_finite_number_raises_at_entry() {
    let (held, registry) = installed();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    for value in ["nan", "inf", "-inf"] {
        let main = format!("main {{ let r = invoke ts.number.ratio({value}) finish r }}");
        let body = stopping(&kit, &registry, "ts.number.ratio", &main, 1_000_000);
        assert!(
            matches!(&body.end, ExpectedEnd::Uncaught(Datum::Error(error)) if error.kind == "RangeError"),
            "ratio({value}): {body:?}"
        );
    }
}

/// Padding to an infinite count appends without end; every append test is
/// charged, so the charge bound stops it, at the same unit in every plan.
#[test]
fn conf_meter_padding_to_an_infinite_count_stops_at_the_charge_bound() {
    let (held, registry) = installed();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let body = stopping(
        &kit,
        &registry,
        "ts.pad",
        "main { let r = invoke ts.pad([], inf) finish r }",
        10_000,
    );
    assert_eq!(
        body.end,
        ExpectedEnd::Bound {
            name: "charge".into(),
            limit: 10_000
        }
    );
}

/// K-CHG-007, guest code after a loop: an aggregate error copies its items,
/// then coerces its message, whose `toString` detaches the items from the
/// arguments the formula measures and raises. The copy was charged per
/// item as it ran, so the detached items cannot make it free.
#[test]
fn conf_meter_loops_before_guest_code_charge_inputs_detached_later() {
    let (held, registry) = installed();
    let registry = Arc::new(registry);
    let kit = Kit::new(held.view(&held.own()).unwrap(), registry.clone());
    let slope = metered_slope(
        &kit,
        &registry,
        &["ts.error.construct", "num.lt", "num.add", "list.len"],
        |n, control| {
            let run = if control {
                "let r = apply f(o, [])"
            } else {
                "let r = invoke ts.error.construct(\"AggregateError\", args, true)"
            };
            format!(
                "main {{ let xs = [] let i = 0 while num.lt(i, {n}) {{ set xs[i] = i set i = num.add(i, 1) }} let args = [xs] \
                 let f = fn(this, a) {{ while num.lt(0, list.len(args)) {{ remove args[0] }} throw \"detached\" }} \
                 let o = {{toString: f}} set args[1] = o try {{ {run} }} catch e {{ }} finish list.len(xs) }}"
            )
        },
    );
    assert!(
        slope >= 56,
        "{slope} more library tests for 56 more copied items"
    );
}
