//! Helper and callback frames enter the common register loop.

use super::*;

/// Helper-frame admission, K-ITER-005, K-CHG-007 and K-EVAL-008.
pub(in crate::laws) const HELPERS: &str = r#"
definition "function captured(x: Any) -> Any\nkernel 1\nmetering statements\nbody { let count = x let reader = fn() { set count = flat.add(count, 1) return (x, count) } do perform 0(\"cell\") as Any return apply reader() }"
definition "function recover() -> Any\nkernel 1\nmetering statements\nbody { try { throw \"failure\" } catch err { let answer = flat.add(6, 1) do perform 0(\"catch\") as Any return answer } finally { do flat.add(3, 4) } }"
definition "function cleanup(failing: Any) -> Any\nkernel 1\nmetering statements\nbody { try { if failing { throw \"departure\" } return 9 } finally { let held = [1, 2] do perform 0(held) as Any } }"
main {
  let bump = fn(x) { return flat.add(x, 1) }
  let unprepared = helper.native(7)
  let appended = helper.append()
  let tags = [helper.tag(7), helper.tag(8), helper.tag("x"), helper.tag("y")]
  let mapped = invoke each.map(bump, [1, 2, 3])
  let twice = invoke each.twice(bump, 1)
  let arithmetic = invoke helper.arithmetic(2)
  let forwarded = invoke helper.forward_next({step: fn(x) { do perform 0("forward") as Any return 7 }})
  let d = {at: 0, step: fn(d) { if flat.lt(d.at, 2) { set d.at = flat.add(d.at, 1) return (d.at,) } return () }}
  let walked = []
  for x in d via (open helper.open, next helper.next, item helper.item) { set walked[list.len(walked)] = x }
  let caught = call recover()
  let returned = call cleanup(false)
  let thrown = null
  try { do call cleanup(true) } catch err { set thrown = err }
  let captured = call captured(4)
  return (tags, appended, unprepared, mapped, twice, arithmetic, forwarded, walked, caught, returned, thrown, captured)
}
"#;

pub(in crate::laws) fn helper_probes(
    registry: &mut lash_kernel_doc::FunctionRegistry,
    ids: &std::collections::BTreeMap<&'static str, lash_vocabulary::names::FunctionId>,
) -> Vec<(&'static str, lash_vocabulary::names::FunctionId)> {
    let mut out = call_probes(registry, ids);
    let add = out.iter().find(|(name, _)| *name == "flat.add").unwrap().1;
    let kind = out.iter().find(|(name, _)| *name == "kind").unwrap().1;
    let same = out.iter().find(|(name, _)| *name == "same").unwrap().1;
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.tag(x: Any) -> Any\nkernel 1\ncharge 1\nnative\nuse kind = @{kind}\nuse same = @{same}\nbody {{ return same(kind(x), \"int\") }}"
    )).unwrap();
    out.push(("helper.tag", registry.register(definition, None).unwrap()));

    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.arithmetic(x: Any) -> Any\nkernel 1\ncharge sum(8, size(x), size(result))\nuse flat.add = @{add}\nbody {{ let y = 0 for n in [x] metered 2 {{ set y = flat.add(n, 1) }} let answer = invoke flat.add(y, 1) return answer }}"
    )).unwrap();
    out.push((
        "helper.arithmetic",
        registry.register(definition, None).unwrap(),
    ));
    out.extend(
        [
            ("helper.open", "x: Any", "return x"),
            (
                "helper.next",
                "d: Any",
                "let step = d.step return apply step(d)",
            ),
            ("helper.item", "d: Any, raw: Any", "return raw"),
        ]
        .into_iter()
        .map(|(name, params, body)| {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function {name}({params}) -> Any\nkernel 1\ncharge 1\nnative\nbody {{ {body} }}"
            ))
            .unwrap();
            (name, registry.register(definition, None).unwrap())
        })
        .collect::<Vec<_>>(),
    );
    let item = out
        .iter()
        .find(|(name, _)| *name == "helper.item")
        .unwrap()
        .1;
    // K-MACH-008: a partial prepared answer retains its ordinary body
    // continuation when another input declines the immediate path.
    let plan = Box::leak(Box::new(lash_kernel_doc::plan::HelperPlan {
        name: "helper.item",
        function: item,
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
            ctx.param(1, 0)?;
            Ok(ctx.take(0))
        },
        template: Some(lash_kernel_doc::plan::HelperTemplate {
            entry: Some(|args, _| match args {
                [_, lash_kernel_doc::Value::Int(0)] => Some(lash_kernel_doc::Value::Int(0)),
                _ => None,
            }),
            metered: None,
            depth: 1,
            callees: &[],
            heap: None,
            forwards: Default::default(),
        }),
    }));
    registry.install_plans(&[plan]).unwrap();
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.forward() -> Any\nkernel 1\ncharge 1\nnative\nuse helper.item = @{item}\nbody {{ let prepared = helper.item(null, 0) return helper.item(null, 7) }}"
    )).unwrap();
    out.push((
        "helper.forward",
        registry.register(definition, None).unwrap(),
    ));
    let next = out
        .iter()
        .find(|(name, _)| *name == "helper.next")
        .unwrap()
        .1;
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.forward_next(d: Any) -> Any\nkernel 1\ncharge sum(1, size(d), size(result))\nnative\nuse helper.next = @{next}\nbody {{ let answer = helper.next(d) return answer }}"
    )).unwrap();
    out.push((
        "helper.forward_next",
        registry.register(definition, None).unwrap(),
    ));
    let length = ids["list.len"];
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.append() -> Any\nkernel 1\ncharge sum(64, size(result))\nnative\nuse list.len = @{length}\nbody {{ let values = [] let value = 7 set values[list.len(values)] = value return values }}\n",
    )).unwrap();
    out.push((
        "helper.append",
        registry.register(definition, None).unwrap(),
    ));
    struct Unprepared;
    impl lash_kernel_doc::NativeFunction for Unprepared {
        fn call(
            &self,
            call: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            match call.args.values()[0] {
                lash_kernel_doc::Value::Int(0) => Err(lash_kernel_doc::NativeError::Decline),
                lash_kernel_doc::Value::Int(-1) => Err(lash_kernel_doc::NativeError::refused(
                    "law_native",
                    "raised before body",
                )),
                value => Ok(value),
            }
        }
    }
    let definition = lash_kernel_doc::parse_definition(
        "function helper.native(x: Any) -> Any\nkernel 1\nerrors \"law_native\"\ncharge sum(3, size(x))\nnative\nbody { return x }\n",
    )
    .unwrap();
    let native = registry
        .register(definition, Some(std::sync::Arc::new(Unprepared)))
        .unwrap();
    out.push(("helper.native", native));
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.forward_native() -> Any\nkernel 1\ncharge 1\nuse helper.native = @{native}\nbody {{ return invoke helper.native(7) }}"
    )).unwrap();
    out.push((
        "helper.forward_native",
        registry.register(definition, None).unwrap(),
    ));
    out
}

/// K-CHG-007 / K-MACH-008: helper and callback frames preserve every
/// charge event and source continuation while entering the common loop.
#[test]
fn helper_frames_agree() {
    use crate::perf_witness::execution::{self, Owner};
    let run = |source: &str, layout, cold| {
        let mut embedder = Embedder::with(
            source,
            Setup {
                native_twice: false,
                probes: Some(helper_probes),
                layout,
                slice: if cold { 7 } else { u64::MAX },
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        embedder.machine.count_execution();
        let mut checkpoints = Vec::new();
        let end = loop {
            match embedder.run() {
                Step::Ended(end) => {
                    assert!(matches!(end, End::Finished(_)), "{end:?}");
                    break format!("{:?}", result(end));
                }
                Step::Parked(_) => {
                    checkpoints.push(embedder.machine.semantic_checkpoint());
                    if cold {
                        embedder.reopen(layout, true);
                        embedder.machine.record_charges(tape.clone());
                    }
                    embedder.deliver_next();
                }
                Step::Slice => {
                    if cold {
                        checkpoints.push(embedder.machine.semantic_checkpoint());
                        embedder.reopen(layout, true);
                        embedder.machine.record_charges(tape.clone());
                    }
                }
            }
        };
        let census = embedder.machine.take_execution_census();
        (
            (
                end,
                embedder.world.printed,
                embedder.requests,
                embedder.machine.meters().charged,
                lash_kernel_exec::Weight::expand(tape.steps()),
                checkpoints,
            ),
            census,
        )
    };
    const PAID_BODY: &str = "main { return helper.forward() }";
    const PINNED_APPEND: &str = "main { return helper.append() }";
    const PINNED_DOCUMENT: &str = r#"definition "function pinned() -> Any\nkernel 1\nmetering statements\nbody { let values = [] let value = 7 set values[list.len(values)] = value set values[num.add(0, 0)] = value return values }"
main { return call pinned() }"#;
    const PAID_ROOT: &str = r#"
definition "function paid() -> Any\nkernel 1\nmetering statements\nbody { let answer = invoke helper.forward() return answer }"
main { return call paid() }
"#;
    const PAID_PARK: &str = r#"
definition "function paid(d: Any) -> Any\nkernel 1\nmetering statements\nbody { return helper.forward_next(d) }"
main { let d = {step: fn(x) { set x.changed = 12 do perform 0("paid") as Any return 7 }} return call paid(d) }
"#;
    const ROOT_CALL: &str = "main { return invoke helper.arithmetic(2) }";
    const CHARGED_ROOT: &str = r#"
definition "function rooted(x: Any) -> Any\nkernel 1\nmetering statements\nbody { let answer = invoke flat.add(x, 1) return answer }"
main { return call rooted(20) }
"#;
    const NESTED_BODY: &str = "main { return invoke helper.forward() }";
    const NESTED_NATIVE: &str = "main { return invoke helper.forward_native() }";
    const NATIVE_EXPR: &str = "main { return helper.native(7) }";
    const ROOT_NATIVE: &str = r#"
definition "function rooted() -> Any\nkernel 1\nmetering statements\nbody { let answer = invoke helper.native(7) return answer }"
main { return call rooted() }
"#;
    const ROOT_BODY: &str =
        "main { let d = {step: fn(x) { return 7 }} return invoke helper.forward_next(d) }";
    const BODY_PARK: &str = r#"main { let d = {step: fn(x) { do perform 0("forward") as Any return 7 }} return invoke helper.forward_next(d) }"#;
    const ROOT_CALLBACK: &str =
        "main { let d = {step: fn(x) { return 7 }} return invoke helper.next(d) }";
    for source in [
        "main { return invoke pair.twice(21) }",
        ROOT_CALL,
        PAID_BODY,
        PINNED_APPEND,
        PINNED_DOCUMENT,
        PAID_ROOT,
        PAID_PARK,
        ROOT_CALLBACK,
        ROOT_BODY,
        NESTED_BODY,
        NESTED_NATIVE,
        ROOT_NATIVE,
        NATIVE_EXPR,
        "main { return helper.native(0) }",
        "main { try { return helper.native(-1) } catch err { return err.kind } }",
        BODY_PARK,
        CHARGED_ROOT,
        HELPERS,
    ] {
        for cold in [false, true] {
            let (reference, _) = run(source, Layout(0).without_regions(), cold);
            let (actual, census) = run(source, Layout(0), cold);
            assert_eq!(
                actual, reference,
                "helper/callback continuation cold={cold}"
            );
            if !cold {
                if source == PAID_BODY {
                    assert_eq!(
                        census
                            .slow_exits
                            .get(&Owner::RegisterProgram)
                            .and_then(|reasons| reasons.get(&execution::Reason::Operator))
                            .copied()
                            .unwrap_or(0),
                        0,
                        "a partial prepared helper must retain its register body: {census:?}"
                    );
                }
                if source == PAID_BODY
                    || source == PAID_ROOT
                    || source == PINNED_APPEND
                    || source == PINNED_DOCUMENT
                {
                    assert_eq!(
                        census
                            .statements
                            .get(&Owner::GeneralStatementDriver)
                            .copied()
                            .unwrap_or(0),
                        0,
                        "paid helper calls must remain in the register loop: {source}; {census:?}"
                    );
                }
                if source == ROOT_CALL
                    || source == PINNED_DOCUMENT
                    || source == PINNED_APPEND
                    || source == ROOT_CALLBACK
                    || source == ROOT_BODY
                    || source == NESTED_BODY
                    || source == NESTED_NATIVE
                    || source == NATIVE_EXPR
                    || source == ROOT_NATIVE
                    || source == CHARGED_ROOT
                {
                    assert_eq!(
                        census
                            .slow_exits
                            .get(&Owner::RegisterProgram)
                            .and_then(|reasons| reasons.get(&execution::Reason::ColdBranch))
                            .copied()
                            .unwrap_or(0),
                        0,
                        "root calls must execute their admitted operation: {source}; {census:?}"
                    );
                }
                assert!(
                    census
                        .statements
                        .get(&Owner::RegisterProgram)
                        .copied()
                        .unwrap_or(0)
                        > 0,
                    "helper statements must execute in the common loop: {source}; {census:?}"
                );
                assert_eq!(
                    census
                        .statements
                        .get(&Owner::SourceHelperFrame)
                        .copied()
                        .unwrap_or(0),
                    0,
                    "a formula-metered helper must use its register program: {census:?}"
                );
            }
        }
    }
}

/// K-BND-003: fresh parameter cells and physical registers use the one
/// entry nursery; a source-owned call may already use most of that room.
#[test]
fn shared_parameters_claim_the_entry_nursery() {
    use crate::perf_witness::execution::{Owner, Reason};
    let width = 1400;
    let params = (0..width)
        .map(|i| format!("p{i}: Any"))
        .collect::<Vec<_>>()
        .join(",");
    let values = (0..width)
        .map(|i| format!("p{i}"))
        .collect::<Vec<_>>()
        .join(",");
    let args = vec!["0"; width].join(",");
    let definition = format!(
        "function shared({params}) -> Any\nkernel 1\nmetering statements\nbody {{ return fn() {{ return ({values}) }} }}"
    );
    let source = format!(
        "definition {}\nmain {{ do perform 0(\"before\") as Any let held = call shared({args}) return apply held() }}",
        serde_json::to_string(&definition).unwrap()
    );
    let run = |layout| {
        let mut embedder = Embedder::with(
            &source,
            Setup {
                layout,
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        embedder.machine.record_charges(tape.clone());
        embedder.machine.count_execution();
        let end = embedder.run_to_end(&[]);
        let census = embedder.machine.take_execution_census();
        (
            result(end),
            lash_kernel_exec::Weight::expand(tape.steps()),
            census,
        )
    };
    let reference = run(Layout(0).without_regions());
    let actual = run(Layout(0));
    assert_eq!((&actual.0, &actual.1), (&reference.0, &reference.1));
    assert_eq!(actual.0, Datum::Tuple(vec![int(0); width]));
    assert!(
        actual
            .2
            .slow_exits
            .get(&Owner::RegisterProgram)
            .and_then(|reasons| reasons.get(&Reason::Capacity))
            .copied()
            .unwrap_or(0)
            > 0,
        "shared parameters must claim nursery room before their call commits: {:?}",
        actual.2
    );
}
