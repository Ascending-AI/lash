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
        for id in package.code().ids() {
            let function = registry.get(id).unwrap();
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
                    function: *id,
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
        "kernel 1\nnumbers by_spelling\neffect echo(value: Any) -> Null\nuse ts.array.forEach = @{id}\nmain {{ let visit = fn(context, args) {{ let value = args[0] do perform echo(value) as Null return null }} let callbacks = [visit] let xs = [1, 2] do invoke ts.array.forEach(xs, callbacks) finish null }}"
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
    let current = package.digest();
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
        assert_eq!(stale.current, current);
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
