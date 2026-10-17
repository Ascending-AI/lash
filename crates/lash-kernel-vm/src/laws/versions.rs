//! PLAN-INDEP for numeric block versions and their declining call guards.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, Float, FunctionId, FunctionRegistry, NativeCall, NativeError, NativeFunction,
    PreparedNative, Value, parse_definition,
};

use super::embedder::{Embedder, ROOMY, Setup, text};
use crate::{Bounds, End, Layout, Machine, Step};

struct Scalar;
impl NativeFunction for Scalar {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        Self::answer(call.args).ok_or(NativeError::Decline)
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            entry: Self::answer,
            operation: "law.scalar",
        })
    }
}
impl Scalar {
    fn answer(args: &[Value]) -> Option<Value> {
        match args {
            [Value::Float(value)] => Some(Value::Float(Float::new(value.get() + 1.0))),
            _ => None,
        }
    }
}

struct IsFloat;
impl NativeFunction for IsFloat {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        Ok(Value::Bool(matches!(call.args, [Value::Float(_)])))
    }
}

struct AddFloat;
impl NativeFunction for AddFloat {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let [Value::Float(a), Value::Float(b)] = call.args else {
            panic!("the law's body guards its float addition")
        };
        Ok(Value::Float(Float::new(a.get() + b.get())))
    }
}

struct IsBool;
impl NativeFunction for IsBool {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        Ok(Value::Bool(matches!(call.args, [Value::Bool(_)])))
    }
}

struct Truth;
impl NativeFunction for Truth {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        Self::answer(call.args).ok_or(NativeError::Decline)
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            entry: Self::answer,
            operation: "law.truth",
        })
    }
}
impl Truth {
    fn answer(args: &[Value]) -> Option<Value> {
        match args {
            [Value::Bool(value)] => Some(Value::Bool(*value)),
            _ => None,
        }
    }
}

struct ListItem;
impl NativeFunction for ListItem {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        Self::answer(call.args).ok_or(NativeError::Decline)
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            entry: Self::answer,
            operation: "law.list_item",
        })
    }
}
impl ListItem {
    fn answer(args: &[Value]) -> Option<Value> {
        match args {
            [Value::List(_), Value::Float(value)] => {
                Some(Value::Float(Float::new(value.get() + 1.0)))
            }
            _ => None,
        }
    }
}

fn probes(
    registry: &mut FunctionRegistry,
    _: &BTreeMap<&'static str, FunctionId>,
) -> Vec<(&'static str, FunctionId)> {
    let mut register = |name: &'static str, params: &str, native: Arc<dyn NativeFunction>| {
        let definition = parse_definition(&format!(
            "function {name}({params}) -> Any\nkernel 1\ncharge 1\nnative\n"
        ))
        .unwrap();
        (name, registry.register(definition, Some(native)).unwrap())
    };
    let is_float = register("law.float", "x: Any", Arc::new(IsFloat));
    let add = register("law.add", "a: Any, b: Any", Arc::new(AddFloat));
    let boolean = register("law.boolean", "x: Any", Arc::new(IsBool));
    let definition = parse_definition(&format!(
        r#"function law.scalar(x: Any) -> Any
kernel 1
charge sum(3, deep(x))
use law.float = @{}
use law.add = @{}
native
body {{
  if law.float(x) {{ return law.add(x, 1.0) }}
  let answered = apply x()
  return answered
}}
"#,
        is_float.1, add.1
    ))
    .unwrap();
    let scalar = registry
        .register(definition, Some(Arc::new(Scalar)))
        .unwrap();
    let definition = parse_definition(&format!("function law.truth(x: Any) -> Any\nkernel 1\ncharge sum(3, deep(x))\nuse law.boolean = @{}\nnative\nbody {{ if law.boolean(x) {{ return x }} let answered = apply x() return answered }}\n", boolean.1)).unwrap();
    let truth = registry
        .register(definition, Some(Arc::new(Truth)))
        .unwrap();
    let definition = parse_definition(
        "function law.open(x: Any) -> Any\nkernel 1\ncharge 1\nnative\nbody { return x }\n",
    )
    .unwrap();
    let open = registry.register(definition, None).unwrap();
    let definition = parse_definition(&format!("function law.item(s: Any, raw: Any) -> Any\nkernel 1\ncharge sum(5, deep(raw))\nuse law.scalar = @{}\nnative\nbody {{ let answered = invoke law.scalar(raw) return answered }}\n", scalar)).unwrap();
    let item = registry
        .register(definition, Some(Arc::new(ListItem)))
        .unwrap();
    vec![
        is_float,
        add,
        boolean,
        ("law.scalar", scalar),
        ("law.truth", truth),
        ("law.open", open),
        ("law.item", item),
    ]
}

const PROGRAM: &str = r#"
fn sibling() { let reply = perform echo("peer") as Any return reply }
fn numeric(n) {
  let pair = (law.scalar(n), perform echo("park") as Any)
  let next = pair[0]
  print next
  let delivered = pair[1]
  set n = fn() { print "fallback" return delivered }
  return law.scalar(n)
}
main {
  let peer = spawn call sibling()
  let value = call numeric(2.0)
  let truth_pair = (law.truth(true), perform echo("truth") as Any)
  print truth_pair[0]
  let fallback = fn() { return false }
  print law.truth(fallback)
  try { let extra = law.scalar(null) } catch err { print err.kind }
  let joined = join peer
  return value
}
"#;

type Trace = (
    String,
    Vec<Datum>,
    Vec<crate::Request>,
    u64,
    Vec<crate::conformance::Checkpoint>,
);

fn observe(program: &str, bounds: Bounds, layout: Layout, cold: bool) -> Trace {
    let mut embedder = Embedder::with(
        program,
        Setup {
            probes: Some(probes),
            bounds,
            layout,
            slice: if cold { 1 } else { u64::MAX },
            ..Setup::default()
        },
    );
    let mut continuations = Vec::new();
    let mut plans = [Layout(0), Layout(2), Layout(1), Layout(3), Layout(0x5eed)]
        .into_iter()
        .cycle();
    let end = loop {
        let step = embedder.run();
        if let Step::Ended(end) = step {
            if let End::Finished(finished) = &end
                && let Some(carry) = &finished.carry
            {
                continuations.push(
                    crate::conformance::session(carry, Box::new(embedder.source.clone())).unwrap(),
                );
            }
            break match end {
                End::Finished(finished) => format!("finished {:?}", finished.result),
                other => format!("{other:?}"),
            };
        }
        if matches!(step, Step::Parked(_)) {
            continuations.push(embedder.machine.semantic_checkpoint());
        }
        if cold {
            embedder.reopen(plans.next().unwrap(), true);
        }
        if matches!(step, Step::Parked(_)) {
            embedder.deliver_next();
        }
    };
    (
        end,
        embedder.world.printed,
        embedder.requests,
        embedder.machine.meters().charged,
        continuations,
    )
}

/// PLAN-INDEP: a numeric entry selects scalar blocks; changing its parameter
/// to a guest closure fails the call guard and executes the complete body.
/// Errors, guest effects and memory refusal are the generic plan's,
/// uninterrupted and cold-restored at each safepoint with empty caches.
#[test]
fn numeric_block_versions_fall_back_after_a_parameter_changes_kind() {
    let reference = observe(PROGRAM, ROOMY, Layout(0), false);
    assert_eq!(reference.0, format!("finished {:?}", text("park")));
    assert_eq!(
        reference.1,
        vec![
            Datum::Float(Float::new(3.0)),
            text("fallback"),
            Datum::Bool(true),
            Datum::Bool(false),
            text("type_error")
        ]
    );
    for program in [
        PROGRAM.to_owned(),
        PROGRAM.replace("return delivered", "return work.fill(50000)"),
    ] {
        let bounds = Bounds {
            memory: 1 << 18,
            ..ROOMY
        };
        let reference = observe(&program, bounds, Layout(0), false);
        for layout in [
            Layout(0),
            Layout(1),
            Layout(2),
            Layout(3),
            Layout(0x5eed),
            Layout(u64::MAX),
        ] {
            if layout != Layout(0) {
                assert_eq!(
                    observe(&program, bounds, layout, false),
                    reference,
                    "{layout:?}"
                );
            }
            assert_eq!(
                observe(&program, bounds, layout, true),
                reference,
                "cold {layout:?}"
            );
        }
    }
}

const LIST_PROGRAM: &str = r#"
fn sibling() { let reply = perform echo("peer") as Any return reply }
main {
  let peer = spawn call sibling()
  let count = 0
  let slow = fn() {
    set count = num.add(count, 1)
    let delivered = perform echo("slow") as Any
    return delivered
  }
  let items = [1.0, slow, 2.0]
  let out = []
  for x in items via (open law.open, item law.item) {
    let delivered = perform echo(x) as Any
    set out[list.len(out)] = delivered
  }
  for x in (3.0,) via (open law.open, item law.item) {
    set out[list.len(out)] = x
  }
  let joined = join peer
  return (out, count)
}
"#;

/// PLAN-INDEP and CONT-ONCE: the fused list handoff answers numeric items,
/// declines a guest callback that parks, and uses the original protocol for
/// a tuple. Neither a decline nor a cold restore advances the cursor twice.
#[test]
fn fused_list_items_decline_and_resume_through_the_original_header() {
    let reference = observe(LIST_PROGRAM, ROOMY, Layout(0), false);
    let expected = Datum::Tuple(vec![
        Datum::List(vec![
            Datum::Float(Float::new(2.0)),
            text("slow"),
            Datum::Float(Float::new(3.0)),
            Datum::Float(Float::new(4.0)),
        ]),
        super::embedder::int(1),
    ]);
    assert_eq!(reference.0, format!("finished {expected:?}"));
    for program in [
        LIST_PROGRAM.to_owned(),
        LIST_PROGRAM.replace("return delivered", "return work.fill(50000)"),
    ] {
        let bounds = Bounds {
            memory: 1 << 18,
            ..ROOMY
        };
        let reference = observe(&program, bounds, Layout(0), false);
        for layout in [
            Layout(0),
            Layout(1),
            Layout(2),
            Layout(3),
            Layout(0x5eed),
            Layout(u64::MAX),
        ] {
            if layout != Layout(0) {
                assert_eq!(
                    observe(&program, bounds, layout, false),
                    reference,
                    "{layout:?}"
                );
            }
            assert_eq!(
                observe(&program, bounds, layout, true),
                reference,
                "cold {layout:?}"
            );
        }
    }
}
