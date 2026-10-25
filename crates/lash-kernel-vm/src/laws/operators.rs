//! PLAN-INDEP for operator opcodes and their declining guards: a guard
//! miss runs the native's general call, then its body.

use std::collections::BTreeMap;
use std::sync::Arc;

use lash_kernel_doc::{
    Datum, Float, FunctionRegistry, NativeCall, NativeError, NativeFunction, PreparedNative, Value,
    parse_definition,
};

use lash_vocabulary::names::FunctionId;

use super::embedder::{Embedder, ROOMY, Setup, text};
use crate::{Bounds, End, Layout, Machine, Step};

struct Scalar;
impl NativeFunction for Scalar {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Self::answer(call.args.values()).ok_or(NativeError::Decline)
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            charged_entry: None,
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
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Ok(Value::Bool(matches!(call.args.values(), [Value::Float(_)])))
    }
}

struct AddFloat;
impl NativeFunction for AddFloat {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let [Value::Float(a), Value::Float(b)] = call.args.values() else {
            panic!("the law's body guards its float addition")
        };
        Ok(Value::Float(Float::new(a.get() + b.get())))
    }
}

struct IsBool;
impl NativeFunction for IsBool {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Ok(Value::Bool(matches!(call.args.values(), [Value::Bool(_)])))
    }
}

struct Truth;
impl NativeFunction for Truth {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Self::answer(call.args.values()).ok_or(NativeError::Decline)
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            charged_entry: None,
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
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        Self::answer(call.args.values()).ok_or(NativeError::Decline)
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            charged_entry: None,
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

struct ValueOnly;
impl NativeFunction for ValueOnly {
    fn call(&self, _: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        panic!("the value-only prepared guard must answer")
    }
    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: Scalar::answer,
            charged_entry: Some(|_| panic!("an uncharged helper produced charge metadata")),
            operation: "law.value_only",
        })
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
    let value_only = register("law.value_only", "x: Any", Arc::new(ValueOnly));
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
    let definition = parse_definition(&format!(
        "function law.uncharged(x: Any) -> Any\nkernel 1\ncharge 7\nuse law.value_only = @{}\nbody {{ return law.value_only(x) }}\n", value_only.1
    )).unwrap();
    let uncharged = registry.register(definition, None).unwrap();
    vec![
        value_only,
        ("law.uncharged", uncharged),
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
definition "function sibling() -> Any\nkernel 1\nmetering statements\nbody {  let reply = perform 0(\"peer\") as Any return reply  }"

definition "function numeric(n: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  let pair = (law.scalar(n), perform 0(\"park\") as Any)\n  let next = pair[0]\n  print next\n  let delivered = pair[1]\n  set n = fn() { print \"fallback\" return delivered }\n  return law.scalar(n)\n }"

main {
  let peer = spawn call sibling()
  let value = call numeric(2.0)
  let truth_pair = (law.truth(true), perform 0("truth") as Any)
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
        embedder.machine.meters().charged.expect("exact charge"),
        continuations,
    )
}

/// PLAN-INDEP: an operator's prepared entry answers a number; changing its
/// parameter to a guest closure misses its guard, and the general call
/// declines to the complete body.
/// Errors, guest effects and memory refusal are the generic plan's,
/// uninterrupted and cold-restored at each safepoint with empty caches. The
/// refusing fill is far past `K-BND-003`'s declared slack, so every plan
/// refuses at the same statement.
#[test]
fn operators_fall_back_to_the_body_after_a_parameter_changes_kind() {
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
definition "function sibling() -> Any\nkernel 1\nmetering statements\nbody {  let reply = perform 0(\"peer\") as Any return reply  }"

main {
  let peer = spawn call sibling()
  let count = 0
  let slow = fn() {
    set count = num.add(count, 1)
    let delivered = perform 0("slow") as Any
    return delivered
  }
  let items = [1.0, slow, 2.0]
  let out = []
  for x in items via (open law.open, item law.item) {
    let delivered = perform 0(x) as Any
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

/// `K-BND-002`, PLAN-INDEP: an operator's call depth is checked before its
/// prepared entry answers. Under every call-depth bound around a recursion
/// whose innermost call is an operator, the operator opcode refuses where
/// the general call and the body refuse, and answers where they answer.
#[test]
fn an_operator_past_the_depth_bound_is_refused_before_its_prepared_entry_answers() {
    const DEEP: &str = r#"
definition "function up(self: Any, n: Any) -> Any\nkernel 1\nmetering statements\nbody { \n  if num.lt(n, 3) { return apply self(self, num.add(n, 1)) }\n  return law.scalar(1.0)\n }"

main { return call up(&up, 0) }
"#;
    let (mut answered, mut refused) = (0, 0);
    for call_depth in 1..9 {
        let bounds = Bounds {
            call_depth,
            ..ROOMY
        };
        let reference = observe(DEEP, bounds, Layout(1), false);
        if reference.0.starts_with("finished") {
            answered += 1;
        } else {
            assert!(reference.0.contains("CallDepth"), "{}", reference.0);
            refused += 1;
        }
        for layout in [Layout(0), Layout(2), Layout(3), Layout(4), Layout(0x5eed)] {
            assert_eq!(
                observe(DEEP, bounds, layout, false),
                reference,
                "call depth {call_depth}, {layout:?}"
            );
        }
    }
    assert!(
        answered > 0 && refused > 0,
        "{answered} answered, {refused} refused"
    );
}

/// K-CHG-007: a helper body evaluates its prepared scalar operation without
/// entering any charge-only guard. Both a general-call fallback and the
/// metadata-producing callback panic, so a successful nonempty result is
/// evidence that the value-only entry ran.
#[test]
fn uncharged_helpers_use_the_value_only_prepared_entry() {
    let trace = observe(
        "main { let value = invoke law.uncharged(2.0) return value }",
        ROOMY,
        Layout(0),
        false,
    );
    assert_eq!(
        trace.0,
        format!("finished {:?}", Datum::Float(Float::new(3.0)))
    );
}

/// FIG-6754: a template-bearing library invoke is a prepared operator site,
/// retaining its call occurrence and typed boundary, rather than generic action dispatch.
#[test]
fn template_bearing_invokes_are_prepared_operator_sites() {
    use lash_kernel_doc::plan::{HelperPlan, HelperTemplate};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static ANSWERED: AtomicUsize = AtomicUsize::new(0);
    let fixture = super::embedder::library(true);
    let mut registry = (*fixture.registry).clone();
    let definition = parse_definition(
        "function law.templated(x: Float) -> Float\nkernel 1\ncharge sum(3, size(x))\nbody { return x }"
    ).unwrap();
    let function = registry.register(definition, None).unwrap();
    let plan = Box::leak(Box::new(HelperPlan {
        name: "law.templated",
        function,
        callees: &[],
        literals: &[],
        fields: &[],
        records: &[],
        registers: 1,
        slots: &[],
        closures: &[],
        sites: &[],
        run: |ctx, _| {
            ctx.begin()?;
            ctx.param(0, 0)?;
            Ok(ctx.take(0))
        },
        template: Some(HelperTemplate {
            metered: None,
            depth: 1,
            callees: &[],
            entry: Some(|args, _| {
                ANSWERED.fetch_add(1, Ordering::Relaxed);
                args.first().copied()
            }),
            heap: None,
            forwards: Default::default(),
        }),
    }));
    registry.install_plans(&[plan]).unwrap();
    let source = format!(
        "{}numbers by_spelling\nmain {{ return (invoke law.templated(2.0),) }}",
        fixture.header
    );
    let mut document = super::embedder::document(&source, &registry);
    document.main = Some(vec![lash_kernel_doc::Stmt::Return {
        value: lash_kernel_doc::Expr::Act(Box::new(lash_kernel_doc::ActExpr {
            kind: lash_kernel_doc::ActKind::Call(lash_kernel_doc::Callee::Local(function)),
            args: vec![lash_kernel_doc::Expr::Literal(
                lash_kernel_doc::Literal::Float(Float::new(2.0)),
            )],
        })),
    }]);
    let helpers =
        Arc::new(lash_kernel_doc::BuildHelpers::new(registry.clone(), [function]).unwrap());
    let program = crate::laws::embedder::program(
        Arc::new(document),
        crate::PreparedLibrary::with_helpers(Arc::new(registry), helpers),
    );
    let admitted = program
        .library
        .admit(&program.document, Default::default(), &program.selection)
        .unwrap_or_else(|_| panic!("the invoke is admitted"));
    let exe = admitted
        .executable(
            &program.document,
            &program.library,
            &program.documents,
            &program.functions,
            &program.instances,
        )
        .unwrap_or_else(|_| panic!("the invoke compiles"));
    let (stream, _, _) = exe.statement(exe.block(exe.code(exe.main.unwrap()).body).stmts[0]);
    let invoke = stream
        .nested
        .iter()
        .find(|nested| {
            matches!(
                nested.kind,
                crate::compile::NestedKind::Call(crate::compile::NestedCallee::CheckedHelper(_))
            )
        })
        .unwrap();
    assert!(
        invoke.operator,
        "a template-bearing invoke must select its prepared operator"
    );
    let mut machine =
        super::embedder::start_machine(program, ROOMY, Setup::default().start).unwrap();
    let mut host = super::embedder::World::default();
    match machine.run(&mut host, u64::MAX).unwrap() {
        Step::Ended(End::Finished(finished)) => {
            assert_eq!(finished.result, Datum::Float(Float::new(2.0)));
        }
        other => panic!("template invoke did not finish: {other:?}"),
    }
    assert_eq!(ANSWERED.load(Ordering::Relaxed), 1);
}

/// FIG-6959 / PLAN-INDEP: statement-root invokes use the same typed kernel
/// as nested operators, in scalar and allocating regions. An operator
/// expression in a call island already uses its typed kernel.
/// Their certified fallback has identical values, occurrence counts and
/// charge events, including an uncharged run and every charge-bound prefix.
#[test]
fn statement_root_operators_use_the_kernel_with_the_certified_charge() {
    use lash_kernel_doc::scalar::{Pair, ScalarOp};
    use std::sync::atomic::{AtomicUsize, Ordering};
    static PREPARED: AtomicUsize = AtomicUsize::new(0);
    struct PairProbe<const PAIRED: bool>;
    impl<const PAIRED: bool> NativeFunction for PairProbe<PAIRED> {
        fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
            let [Value::Int(a), Value::Int(b)] = call.args.values() else {
                return Err(NativeError::Decline);
            };
            lash_kernel_doc::scalar::ints(ScalarOp::Add, *a, *b).ok_or(NativeError::Decline)
        }
        fn prepare(&self) -> Option<PreparedNative> {
            Some(PreparedNative {
                pair: PAIRED.then_some(Pair {
                    floats: None,
                    ints: Some(ScalarOp::Add),
                }),
                entry: |args| {
                    PREPARED.fetch_add(1, Ordering::Relaxed);
                    let [Value::Int(a), Value::Int(b)] = args else {
                        return None;
                    };
                    lash_kernel_doc::scalar::ints(ScalarOp::Add, *a, *b)
                },
                charged_entry: None,
                operation: "law.pair",
            })
        }
    }
    fn register<const PAIRED: bool>(
        registry: &mut FunctionRegistry,
        _: &BTreeMap<&'static str, FunctionId>,
    ) -> Vec<(&'static str, FunctionId)> {
        let definition = parse_definition("function law.pair(a: Any, b: Any) -> Any\nkernel 1\ncharge sum(5, size(a), size(b), size(result))\nnative\n").unwrap();
        vec![(
            "law.pair",
            registry
                .register(definition, Some(Arc::new(PairProbe::<PAIRED>)))
                .unwrap(),
        )]
    }
    fn run_pair<const PAIRED: bool>(
        source: &str,
        bounds: Bounds,
        record: bool,
        layout: Layout,
    ) -> (
        String,
        Option<u64>,
        Vec<lash_kernel_exec::Weight>,
        Vec<crate::conformance::Checkpoint>,
    ) {
        let mut embedder = Embedder::with(
            source,
            Setup {
                probes: Some(register::<PAIRED>),
                bounds,
                layout,
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        if record {
            embedder.machine.record_charges(tape.clone());
        }
        let mut checkpoints = Vec::new();
        let end = loop {
            match embedder.run() {
                Step::Ended(end) => {
                    break match end {
                        End::Finished(finished) => format!("finished {:?}", finished.result),
                        other => format!("{other:?}"),
                    };
                }
                Step::Parked(_) => {
                    checkpoints.push(embedder.machine.semantic_checkpoint());
                    embedder.deliver_next();
                }
                Step::Slice => panic!("an unbounded slice yielded"),
            }
        };
        let steps = tape
            .steps()
            .into_iter()
            .flat_map(|weight| match weight {
                lash_kernel_exec::Weight::Units(n) => {
                    vec![lash_kernel_exec::Weight::Units(1); n as usize]
                }
                atomic => vec![atomic],
            })
            .collect();
        (end, embedder.machine.meters().charged, steps, checkpoints)
    }
    for source in [
        "main { let i = 0 let s = 0 while num.lt(i, 5) { set s = invoke law.pair(s, i) set i = num.add(i, 1) } let answer = perform 0(s) as Any return answer }",
        "main { let out = [] let i = 0 let s = 0 while num.lt(i, 5) { set s = invoke law.pair(s, i) set out[list.len(out)] = s set i = num.add(i, 1) } let answer = perform 0(s) as Any return answer }",
        "definition \"function sum(a: Any, b: Any) -> Any\\nkernel 1\\nmetering statements\\nbody {  let s = law.pair(a, b) return s  }\"\n main { let s = call sum(3, 7) let answer = perform 0(s) as Any return answer }",
    ] {
        for charge in [crate::Charge::Exact, crate::Charge::Off] {
            let bounds = Bounds { charge, ..ROOMY };
            for record in [false, true] {
                PREPARED.store(0, Ordering::Relaxed);
                let expected = run_pair::<false>(source, bounds, record, Layout(0));
                assert!(
                    PREPARED.load(Ordering::Relaxed) > 0,
                    "the certified path is exercised: {source}"
                );
                PREPARED.store(0, Ordering::Relaxed);
                let actual = run_pair::<true>(source, bounds, record, Layout(0));
                assert_eq!(actual, expected, "{source}, {charge:?}, tape {record}");
                assert_eq!(
                    PREPARED.load(Ordering::Relaxed),
                    0,
                    "statement-root invokes bypass the fused node: {source}"
                );
                if record && charge == crate::Charge::Exact {
                    let charged = expected.1.unwrap();
                    for charge_limit in 0..=charged {
                        let bounds = Bounds {
                            charge_limit,
                            ..bounds
                        };
                        assert_eq!(
                            run_pair::<true>(source, bounds, true, Layout(0)),
                            run_pair::<false>(source, bounds, true, Layout(0)),
                            "{source}, bound {charge_limit}"
                        );
                    }
                }
            }
        }
    }
}
