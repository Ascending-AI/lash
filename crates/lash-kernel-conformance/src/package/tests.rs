//! CONF-NATIVE, CONF-SHAPE and CONF-METER over installed packages.
#![allow(clippy::unwrap_used, reason = "a malformed law fixture fails its law")]

use std::sync::Arc;

use lash_kernel_doc::Datum;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::Implementation;
use lash_kernel_doc::NativeCall;
use lash_kernel_doc::NativeError;
use lash_kernel_doc::NativeFunction;
use lash_kernel_doc::Value;
use lash_kernel_value::types::Type;

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
    let document = crate::document_fixture::authored(&case.document, registry);
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
fn conf_native_helper_documents() {
    let (held, registry) = installed();
    let view = Arc::clone(&held);
    let mut both = 0;
    let mut native_only = 0;
    let registry = Arc::new(registry);
    let kit = Kit::new(view, registry.clone());
    let mut witnesses = vec![];
    for id in held.functions.keys().copied() {
        let function = registry.get(&id).unwrap();
        let definition = function.definition();
        match &definition.implementation {
            Implementation::Native { .. } => {
                native_only += 1;
                continue;
            }
            Implementation::Guest { .. } => continue,
            Implementation::Both { .. } => {}
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
                "kernel 1\nnumbers by_spelling\n# local definition {} @{id}\nmain {{ let answer = invoke {}({args}) finish answer }}",
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
    let checked = kit.check_all(&witnesses).unwrap();
    println!("CONF-NATIVE: Both={both} native-only={native_only} witnesses={checked}");

    // FIG-7327: empty bytes terminate on bytes.at's index error and pay
    // the helper's formula, including through the reference body.
    let empty_bytes = close(
        case(
            "empty bytes length",
            "kernel 1\nnumbers by_spelling\nmain { return invoke py.bytes_len(b\"\") }".into(),
        ),
        &registry,
    );
    for (library, layout) in [
        (&kit.installed, lash_kernel_vm::kernel_internals::Layout(0)),
        (&kit.bodies, lash_kernel_vm::kernel_internals::Layout(3)),
    ] {
        let (observed, _) = super::observe(library, &empty_bytes, layout).unwrap();
        assert_eq!(observed.end, ExpectedEnd::Finished(Datum::Int(0.into())));
        assert_eq!(observed.charged, Some(9));
    }
}

struct Wrong;
impl NativeFunction for Wrong {
    fn call(&self, _: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Ok(Value::Null)
    }
}

/// The kit rejects a wrong native, naming the value difference.
#[test]
fn conf_native_wrong_native_names_the_difference() {
    let definition = lash_kernel_doc::parse_definition(
        "function planted.answer() -> Int\nkernel 1\ncharge 2\nnative\nbody { return 42 }",
    )
    .unwrap();
    let id = definition.identity();
    let mut document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    document.functions.insert(id, definition.clone());
    let held = Arc::new(document);
    let mut registry = FunctionRegistry::declarations();
    registry
        .register(definition, Some(Arc::new(Wrong)))
        .unwrap();
    let registry = Arc::new(registry);
    let kit = Kit::new(Arc::clone(&held), registry.clone());
    let witness = Witness {
        function: id,
        law: Law::Native,
        case: close(
            case(
                "wrong native",
                format!(
                    "kernel 1\nnumbers by_spelling\n# local definition planted.answer @{id}\nmain {{ let answer = invoke planted.answer() finish answer }}"
                ),
            ),
            &registry,
        ),
    };
    let error = kit.check(&witness).unwrap_err().to_string();
    assert!(
        error.contains("CONF-NATIVE")
            && error.contains("planted.answer")
            && error.contains("value or error differs"),
        "{error}"
    );
}

#[test]
fn conf_meter_unbounded_performing_loop_names_the_difference() {
    let definition = lash_kernel_doc::parse_definition("function planted.polling() -> Null\nkernel 1\ncharge 2\nimport 0 operation planted.poll() -> Bool\nbody { let again = true while again { set again = perform 0() as Bool } return null }").unwrap();
    let mut document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
    document.functions.insert(definition.identity(), definition);
    let held = Arc::new(document);
    let error = super::audit_metering(&held, &super::LoopBounds::new())
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("CONF-METER")
            && error.contains("planted.polling")
            && error.contains("declare metered N"),
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
        "kernel 1\nnumbers by_spelling\nimport 0 operation host.echo(value: Any) -> Null\n# local definition ts.array.forEach @{id}\nmain {{ let visit = fn(context, args) {{ let value = args[0] do perform 0(value) as Null return null }} let callbacks = [visit] let xs = [1, 2] do invoke ts.array.forEach(xs, callbacks) finish null }}"
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
    let kit = Kit::new(Arc::clone(&held), registry);
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

/// Runs `main` with `uses` under the reference bodies and the installed
/// code, which must agree in every observation and charge event. Returns
/// the reference observations and how many two-unit events it charged.
fn metered_events(
    kit: &Kit,
    registry: &FunctionRegistry,
    uses: &[&str],
    main: &str,
) -> (crate::Observations, usize) {
    let uses = uses
        .iter()
        .map(|name| {
            format!(
                "# local definition {name} @{}\n",
                registry.named(name).next().unwrap()
            )
        })
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
    kit: &Kit,
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
    let kit = Kit::new(Arc::clone(&held), registry.clone());
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
    let kit = Kit::new(Arc::clone(&held), registry.clone());
    for traversal in TRAVERSALS {
        let uses = traversal
            .uses()
            .iter()
            .map(|name| {
                format!(
                    "# local definition {name} @{}\n",
                    registry.named(name).next().unwrap()
                )
            })
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
    let kit = Kit::new(Arc::clone(&held), registry.clone());
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
    let kit = Kit::new(Arc::clone(&held), registry.clone());
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
                "definition \"function one() -> Any\\nkernel 1\\nmetering statements\\nimport 0 operation host.echo(x: Any) -> Any\\nbody {{ do yield return 1 }}\"\n\ndefinition \"function bad() -> Any\\nkernel 1\\nmetering statements\\nimport 0 operation host.echo(x: Any) -> Any\\nbody {{ do yield throw \\\"bad\\\" }}\"\n\ndefinition \"function spawner() -> Any\\nkernel 1\\nmetering statements\\nimport 0 operation host.echo(x: Any) -> Any\\nbody {{ do yield let b = spawn call bad() return b }}\"\n\nmain {{ let hs = [] let i = 0 while num.lt(i, {n}) {{ set hs[i] = spawn call one() set i = num.add(i, 1) }} let s = spawn call spawner() {run} finish i }}"
            )
        },
    );
    assert_eq!(slope, 56, "more library tests for 56 more results");
}

/// Runs `main` under the reference bodies and the installed code on a
/// thread, failing the law if either does not stop within a minute. The
/// two must agree in every observation and charge event.
fn stopping(
    kit: &Kit,
    registry: &Arc<FunctionRegistry>,
    helper: &str,
    main: &str,
    charge: u64,
) -> crate::Observations {
    let id = registry.named(helper).next().unwrap();
    let mut case = close(
        case(
            main,
            format!("kernel 1\nnumbers by_spelling\n# local definition {helper} @{id}\n{main}"),
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
    let kit = Kit::new(Arc::clone(&held), registry.clone());
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
    let kit = Kit::new(Arc::clone(&held), registry.clone());
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
    let kit = Kit::new(Arc::clone(&held), registry.clone());
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
