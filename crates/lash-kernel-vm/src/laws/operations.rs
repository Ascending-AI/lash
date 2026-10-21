//! Operations (FIG-6440): a library function with a charge formula, a
//! partial native and a complete kernel body. The native answers what it
//! can and declines the rest to the body (`K-LIB-006`); the body may apply
//! a closure it reaches through its arguments, as an ordinary frame
//! (`K-LIB-004`); the formula charges the operation's own work whichever
//! path runs, and a guest frame the body applies is charged as it runs
//! (`K-CHG-007`). Which path answers is invisible to a run: PLAN-INDEP.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, FunctionRegistry, NativeCall, NativeError, NativeFunction, Value, parse_definition,
};

use lash_vocabulary::names::FunctionId;

use super::embedder::{Embedder, ROOMY, Setup, int, text};
use crate::{Bound, BoundExceeded, Bounds, End, Layout, RunError, Step};

/// `op.step`'s native: an integer gives the next one and a text is
/// doubled, as the library's `text.concat` of it with itself; anything
/// else is declined to the body, which applies it.
struct Step1;

impl NativeFunction for Step1 {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match call.args.values() {
            [Value::Int(value)] => value
                .checked_add(1)
                .map(Value::Int)
                .ok_or(NativeError::Decline),
            [text @ (Value::Text(_) | Value::LongText(_))] => {
                let Some(text) = call.args.text(text)? else {
                    return Err(NativeError::Malformed("a text names another body"));
                };
                let text = text.whole()?;
                Ok(call.new.text(&format!("{text}{text}")))
            }
            _ => Err(NativeError::Decline),
        }
    }
}

/// `op.fill`'s native: the library's own `work.fill`, for an integer.
struct Fill1 {
    fill: Arc<dyn NativeFunction>,
}

impl NativeFunction for Fill1 {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match call.args.values() {
            [Value::Int(_)] => self.fill.call(call),
            _ => Err(NativeError::Decline),
        }
    }
}

/// Whether the one argument is of the kind the function names.
struct Is(fn(&Value) -> bool);

impl NativeFunction for Is {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Ok(Value::Bool(call.args.get(0).is_some_and(self.0)))
    }
}

/// The probes: `probe.is_int` and `probe.is_text`, and two operations:
/// `op.step(x)`, which gives `x + 1` for an integer, `x` twice for a text,
/// and for anything else what applying `x` gives, and `op.fill(n)`, a list
/// of `n` nulls. Their natives, [`Step1`] and [`Fill1`], are registered
/// when `native` says so.
fn register(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
    native: bool,
) -> Vec<(&'static str, FunctionId)> {
    let mut is = |name: &'static str, test: fn(&Value) -> bool| {
        let definition = parse_definition(&format!(
            "function {name}(x: Any) -> Bool\nkernel 1\ncharge 1\nnative\n"
        ))
        .unwrap();
        (
            name,
            registry
                .register(definition, Some(Arc::new(Is(test))))
                .unwrap(),
        )
    };
    let is_int = is("probe.is_int", |value| matches!(value, Value::Int(_)));
    let is_text = is("probe.is_text", |value| matches!(value, Value::Text(_)));
    let step = parse_definition(&format!(
        "function op.step(x: Any) -> Any\nkernel 1\ncharge sum(3, size(x))\n\
         use num.add = @{}\nuse text.concat = @{}\nuse probe.is_int = @{}\nuse probe.is_text = @{}\n\
         native\nbody {{\n  if probe.is_int(x) {{ return num.add(x, 1) }}\n\
         if probe.is_text(x) {{ return text.concat(x, x) }}\n  let y = apply x()\n  return y\n}}\n",
        ids["num.add"], ids["text.concat"], is_int.1, is_text.1
    ))
    .unwrap();
    let step = registry.register(step, None).unwrap();
    let fill = parse_definition(&format!(
        "function op.fill(n: Any) -> Any\nkernel 1\ncharge sum(1, size(n))\nuse work.fill = @{}\n\
         native\nbody {{ return work.fill(n) }}\n",
        ids["work.fill"]
    ))
    .unwrap();
    let fill = registry.register(fill, None).unwrap();
    if native {
        registry.implement(step, Arc::new(Step1)).unwrap();
        let work = registry.native_named("work.fill").unwrap();
        registry
            .implement(fill, Arc::new(Fill1 { fill: work }))
            .unwrap();
    }
    let scheduled = parse_definition(&format!(
        "function op.scheduled(x: Any) -> Any\nkernel 1\ncharge sum(31, size(x), size(result))\nuse op.step = @{step}\nbody {{ let y = invoke op.step(x) return y }}\n"
    ))
    .unwrap();
    let scheduled = registry.register(scheduled, None).unwrap();
    let dynamic = parse_definition(&format!(
        "function op.dynamic(n: Any) -> Any\nkernel 1\ncharge 17\nuse num.lt = @{}\nuse num.add = @{}\nbody {{ let i = 0 while num.lt(i, n) metered 7 {{ set i = num.add(i, 1) }} return i }}\n",
        ids["num.lt"], ids["num.add"]
    )).unwrap();
    let canonical = lash_kernel_doc::print_definition(&dynamic);
    assert_eq!(parse_definition(&canonical).unwrap(), dynamic);
    let dynamic = registry.register(dynamic, None).unwrap();
    let wrapper = parse_definition(&format!("function op.meter_wrapper(n: Any) -> Any\nkernel 1\ncharge 23\nuse op.dynamic = @{dynamic}\nbody {{ let out = invoke op.dynamic(n) return out }}\n")).unwrap();
    let wrapper = registry.register(wrapper, None).unwrap();
    let launch = parse_definition(&format!("function op.launch(x: Any) -> Any\nkernel 1\ncharge 0\nuse op.scheduled = @{scheduled}\nbody {{ let task = spawn invoke op.scheduled(x) let value = join task return value }}\n")).unwrap();
    let launch = registry.register(launch, None).unwrap();
    let launch_native = parse_definition(&format!("function op.launch_native(x: Any) -> Any\nkernel 1\ncharge 0\nuse op.step = @{step}\nbody {{ let task = spawn invoke op.step(x) let value = join task return value }}\n")).unwrap();
    let launch_native = registry.register(launch_native, None).unwrap();
    vec![
        ("op.launch_native", launch_native),
        ("op.launch", launch),
        ("op.meter_wrapper", wrapper),
        ("op.dynamic", dynamic),
        is_int,
        is_text,
        ("op.step", step),
        ("op.fill", fill),
        ("op.scheduled", scheduled),
    ]
}

fn with_native(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    register(registry, ids, true)
}

fn body_only(
    registry: &mut FunctionRegistry,
    ids: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    register(registry, ids, false)
}

/// Each path of `op.step` from inside an expression and as a statement:
/// the native's integer and text, and closures it declines, which the body
/// applies. The closure prints and counts its calls; a park falls between
/// them, and a null the body cannot apply raises inside a `try`.
const PROGRAM: &str = r#"
main {
  let n = op.step(41)
  let m = invoke op.step(n)
  let count = 0
  let bump = fn() {
    set count = num.add(count, 1)
    print count
    return count
  }
  let a = op.step(bump)
  let t = op.step("ab")
  let e = perform 0(t) as Any
  let b = invoke op.step(bump)
  let kind = "none"
  try {
    let bad = op.step(null)
  } catch err {
    set kind = err.kind
  }
  return (n, m, a, b, e, kind, count)
}
"#;

/// How a run ended: a finished run's result, finish and the bindings it did
/// not carry, or any other end. The carry's page identities are left out,
/// for a run reopened cold may lay its heap out on other pages.
type Ended = Result<(Datum, bool, Vec<(lash_vocabulary::names::Name, String)>), Box<End>>;

/// What a law compares of a run: how it ended, what it printed, the
/// requests it made, and its charge.
type Observed = (
    Ended,
    Vec<Datum>,
    Vec<crate::Request>,
    u64,
    Vec<crate::conformance::Checkpoint>,
);

fn ended(end: End) -> Ended {
    match end {
        End::Finished(finished) => Ok((finished.result, finished.finish, finished.not_carried)),
        other => Err(Box::new(other)),
    }
}

/// A plan: the layout the run starts in, whether `op.step`'s native is
/// registered, and, when the run is sealed and reopened at every safe
/// point of a one-unit slice, the layouts it is reopened in by turns.
struct Plan {
    layout: Layout,
    native: bool,
    reopen: Option<[Layout; 3]>,
}

fn observe(program: &str, bounds: Bounds, plan: &Plan) -> Observed {
    let probes: super::embedder::Probes = if plan.native { with_native } else { body_only };
    let mut embedder = Embedder::with(
        program,
        Setup {
            bounds,
            layout: plan.layout,
            slice: if plan.reopen.is_some() { 1 } else { u64::MAX },
            probes: Some(probes),
            ..Setup::default()
        },
    );
    let mut turns = plan.reopen.iter().flatten().copied().cycle();
    let mut checkpoints = Vec::new();
    let end = loop {
        match embedder.run() {
            Step::Ended(end) => break end,
            Step::Slice => {
                if let Some(layout) = turns.next() {
                    embedder.reopen(layout, true);
                }
            }
            Step::Parked(_) => {
                checkpoints.push(embedder.machine.semantic_checkpoint());
                if let Some(layout) = turns.next() {
                    embedder.reopen(layout, true);
                }
                embedder.deliver_next();
            }
        }
    };
    if let End::Finished(finished) = &end
        && let Some(carry) = &finished.carry
    {
        checkpoints
            .push(crate::conformance::session(carry, Box::new(embedder.source.clone())).unwrap());
    }
    let charged = crate::Machine::meters(&embedder.machine)
        .charged
        .expect("exact charge");
    (
        ended(end),
        embedder.world.printed,
        embedder.requests,
        charged,
        checkpoints,
    )
}

/// The plans every law compares: the native's fast path and its general
/// call, every native declining, a shuffled layout, no native registered,
/// and the run sealed and reopened at every safe point under layouts that
/// change the path by turns.
fn plans() -> Vec<Plan> {
    let plan = |layout, native| Plan {
        layout: Layout(layout),
        native,
        reopen: None,
    };
    let turns = [Layout(3), Layout(1), Layout(0x5eed)];
    vec![
        plan(0, true),
        plan(1, true),
        plan(3, true),
        plan(0x5eed, true),
        plan(u64::MAX, true),
        plan(0, false),
        Plan {
            layout: Layout(0),
            native: true,
            reopen: Some(turns),
        },
        Plan {
            layout: Layout(3),
            native: true,
            reopen: Some(turns),
        },
    ]
}

/// PLAN-INDEP (`K-LIB-004`, `K-LIB-006`, `K-CHG-007`): an operation whose
/// native answers, declines, or is not registered gives equal values,
/// errors, prints, effect identities and charge, inside an expression or
/// as a statement, uninterrupted or sealed and reopened at every safe
/// point under another path. The closure the body applies is charged as it
/// runs, beside the formula.
#[test]
fn an_operation_runs_alike_whichever_path_answers() {
    let plans = plans();
    let natural = observe(PROGRAM, ROOMY, &plans[0]);
    assert_eq!(
        natural.0.clone().map(|(result, ..)| result),
        Ok(Datum::Tuple(vec![
            int(42),
            int(43),
            int(1),
            int(2),
            text("abab"),
            text("type_error"),
            int(2),
        ]))
    );
    assert_eq!(
        (natural.1.clone(), natural.2.len()),
        (vec![int(1), int(2)], 1)
    );
    for (index, plan) in plans.iter().enumerate().skip(1) {
        assert_eq!(observe(PROGRAM, ROOMY, plan), natural, "plan {index}");
    }
}

/// `K-CHG-007`: the formula is the operation's own work, and a guest
/// closure its body applies is charged as it runs: a closure that loops
/// 200 times costs the run more than 400 beyond the formula, whichever
/// path declines to the body.
#[test]
fn a_closure_an_operation_applies_is_charged_as_it_runs() {
    let program = |call: &str| {
        format!(
            "main {{ let spin = fn() {{ let i = 0 while num.lt(i, 200) {{ set i = num.add(i, 1) }} \
             return i }} let r = {call} return r }}"
        )
    };
    for plan in plans() {
        let applied = observe(&program("op.step(spin)"), ROOMY, &plan);
        let skipped = observe(&program("200"), ROOMY, &plan);
        assert_eq!(applied.0, skipped.0);
        assert!(
            applied.3 > skipped.3 + 400,
            "{} vs {}",
            applied.3,
            skipped.3
        );
    }
}

/// K-BND-003, conservative: a list the operation builds past the memory
/// bound ends every plan in a typed memory refusal at that bound, never a
/// completion. Plans may observe the bound one declared slack apart; the
/// 50000-item list is far past it, so whichever path builds it refuses at
/// the same statement, with the same charge and nothing printed after it.
#[test]
fn an_operation_past_the_memory_bound_is_refused_under_every_plan() {
    let program = r#"
main {
  let small = op.fill(10)
  print "built"
  let big = op.fill(50000)
  print "too big"
  return 0
}
"#;
    let bounds = Bounds {
        memory: 1 << 18,
        ..ROOMY
    };
    let plans = plans();
    let natural = observe(program, bounds, &plans[0]);
    assert_eq!(natural.1, vec![text("built")], "{:?}", natural.0);
    for (index, plan) in plans.iter().enumerate() {
        let observed = observe(program, bounds, plan);
        assert!(
            matches!(
                observed.0.as_ref().map_err(|end| end.as_ref()),
                Err(End::Error(RunError::Bound(BoundExceeded {
                    bound: Bound::Memory,
                    limit,
                    ..
                }))) if *limit == bounds.memory
            ),
            "plan {index}: {:?}",
            observed.0
        );
        assert_eq!(observed, natural, "plan {index}");
    }
}

/// PLAN-INDEP / K-BND-002: entering an operation occupies one depth unit
/// before either implementation runs. Its body represents that same unit.
#[test]
fn operation_depth_is_reserved_before_either_implementation_answers() {
    for expression in ["op.step(41)", "op.fill(10)", "op.step(fn() { return 7 })"] {
        let program = format!(
            "main {{ let probe = fn() {{ return {expression} }} let out = apply probe() return out }}"
        );
        for depth in 1..=4 {
            let bounds = Bounds {
                call_depth: depth,
                ..ROOMY
            };
            let plans = plans();
            let reference = observe(&program, bounds, &plans[0]);
            for (index, plan) in plans.iter().enumerate().skip(1) {
                assert_eq!(
                    observe(&program, bounds, plan),
                    reference,
                    "{expression}, depth={depth}, plan {index}"
                );
            }
        }
    }
}

/// K-CHG-003/007: a body-only helper bills its formula at completion,
/// including a zero-sized result on raise. Its nested calls add no charge;
/// guest callbacks still do, also after cold reopen under a different plan.
#[test]
fn every_helper_bills_its_formula_including_callbacks_and_raises() {
    for (argument, difference) in [
        ("41", 30),
        ("\"ab\"", 33),
        ("fn() { print 7 return 7 }", 30),
        ("null", 28),
    ] {
        let program = |name| format!("main {{ let r = invoke {name}({argument}) return r }}");
        for plan in plans() {
            let direct = observe(&program("op.step"), ROOMY, &plan);
            let helper = observe(&program("op.scheduled"), ROOMY, &plan);
            assert_eq!(helper.0, direct.0, "{argument}");
            assert_eq!(helper.1, direct.1, "{argument}");
            assert_eq!(helper.3, direct.3 + difference, "{argument}");
        }
    }
}

/// K-CHG-003/007/008: a declared dynamic-loop schedule charges every test,
/// even within a nested library body. Every threshold, including immediately
/// below, at and above each test and completion, stops at the same event
/// under native, body and cold plans.
#[test]
fn metered_tests_and_completion_have_plan_independent_bound_events() {
    let plans = plans();
    for name in ["op.dynamic", "op.meter_wrapper"] {
        let program =
            |n| format!("main {{ let r = invoke {name}({n}) print \"completed\" return r }}");
        let empty = observe(&program(0), ROOMY, &plans[0]);
        let full = observe(&program(3), ROOMY, &plans[0]);
        assert_eq!(full.3 - empty.3, 3 * 7 + 1); // result 3 has one integer word; zero has none
        for limit in 0..=full.3 + 1 {
            let bounds = Bounds {
                charge_limit: limit,
                ..ROOMY
            };
            let reference = observe(&program(3), bounds, &plans[0]);
            for plan in plans.iter().skip(1) {
                assert_eq!(
                    observe(&program(3), bounds, plan),
                    reference,
                    "charge={limit}"
                );
            }
        }
        let bounded = observe(
            &program(1000000000),
            Bounds {
                charge_limit: 40,
                ..ROOMY
            },
            &plans[0],
        );
        assert!(matches!(
            bounded.0.as_ref().map_err(|end| end.as_ref()),
            Err(End::Error(RunError::Bound(BoundExceeded {
                bound: Bound::Charge,
                ..
            })))
        ));
        assert!(bounded.1.is_empty());
    }
}

/// K-CHG-003/007: a spawned library root bills its own formula, even
/// from a library caller, with equal bound events under native/body/cold plans.
#[test]
fn spawned_library_roots_keep_their_formula_arguments() {
    let plans = plans();
    for (helper, difference) in [("op.launch", 3 + 6 + 6), ("op.launch_native", 3 + 6)] {
        let program = |arg| format!("main {{ let r = invoke {helper}(\"{arg}\") return r }}");
        let short = observe(&program("a"), ROOMY, &plans[0]);
        let long = observe(&program("abcd"), ROOMY, &plans[0]);
        assert_eq!(long.3 - short.3, difference, "{helper}");
        for limit in 0..=long.3 + 1 {
            let bounds = Bounds {
                charge_limit: limit,
                ..ROOMY
            };
            let reference = observe(&program("abcd"), bounds, &plans[0]);
            for plan in &plans[1..] {
                assert_eq!(
                    observe(&program("abcd"), bounds, plan),
                    reference,
                    "{helper}, charge={limit}"
                );
            }
        }
    }
}
