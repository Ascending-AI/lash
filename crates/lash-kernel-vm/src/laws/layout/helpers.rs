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
    ids: &std::collections::BTreeMap<&'static str, lash_kernel_value::names::FunctionId>,
) -> Vec<(&'static str, lash_kernel_value::names::FunctionId)> {
    let mut out = call_probes(registry, ids);
    for (definition, native) in lash_kernel_lib::text_json()
        .into_iter()
        .filter(|(definition, _)| definition.name.as_str() == "text.join")
    {
        out.push((
            "text.join",
            registry.register(definition, Some(native)).unwrap(),
        ));
    }
    for (definition, native) in lash_kernel_lib::numbers()
        .into_iter()
        .filter(|(definition, _)| definition.name.as_str() == "num.to_text")
    {
        out.push((
            "num.to_text",
            registry.register(definition, Some(native)).unwrap(),
        ));
    }
    for (name, signature) in [
        ("num.min", "a: Number, b: Number) -> Number"),
        ("bool.not", "value: Bool) -> Bool"),
        ("list.get", "values: List(Any), index: Int) -> Any"),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}({signature}\nkernel 1\ncharge 1\nnative\n"
        ))
        .unwrap();
        out.push((
            name,
            registry
                .register(definition, lash_kernel_lib::native(name))
                .unwrap(),
        ));
    }
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
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.reduce(values: List(Any)) -> Any\nkernel 1\ncharge sum(4, deep(values))\nuse flat.add = @{add}\nbody {{ let total = 0 for value in values metered 2 {{ set total = flat.add(total, value) }} return total }}"
    )).unwrap();
    out.push((
        "helper.reduce",
        registry.register(definition, None).unwrap(),
    ));
    let length = ids["list.len"];
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.copy(values: List(Any)) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\nuse list.len = @{length}\nbody {{ let out = [] for value in values metered 2 {{ set out[list.len(out)] = value }} return out }}"
    )).unwrap();
    out.push(("helper.copy", registry.register(definition, None).unwrap()));
    let addition = add;
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.place(values: List(Any), target: List(Any), position: Int) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\nuse flat.add = @{addition}\nbody {{ for value in values metered 2 {{ set target[position] = value set position = flat.add(position, 1) }} return (target, position) }}"
    )).unwrap();
    out.push(("helper.place", registry.register(definition, None).unwrap()));
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.visit(values: List(Any), bound: Int) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\nuse same = @{same}\nuse flat.add = @{add}\nuse list.len = @{length}\nbody {{ let out = [] let i = 0 for value in values metered 2 {{ if same(i, bound) {{ break }} if same(value, ()) {{ set i = flat.add(i, 1) }} else {{ set out[list.len(out)] = value }} set i = flat.add(i, 1) }} return (out, i) }}"
    )).unwrap();
    out.push(("helper.visit", registry.register(definition, None).unwrap()));
    struct Lookalike;
    impl Lookalike {
        fn answer(args: &[lash_kernel_doc::Value]) -> Option<lash_kernel_doc::Value> {
            use lash_kernel_doc::Value;
            match args {
                [value @ Value::Bool(_)] => Some(*value),
                [Value::Int(a), Value::Int(b)] => a.checked_add(*b).map(Value::Int),
                _ => None,
            }
        }
    }
    impl lash_kernel_doc::NativeFunction for Lookalike {
        fn call(
            &self,
            call: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            Self::answer(call.args.values()).ok_or(lash_kernel_doc::NativeError::Decline)
        }
        fn prepare(&self) -> Option<lash_kernel_doc::PreparedNative> {
            Some(lash_kernel_doc::PreparedNative {
                pair: None,
                entry: Self::answer,
                charged_entry: None,
                operation: "law.lookalike",
            })
        }
    }
    for (alias, declaration) in [
        ("law.not", "bool.not(value: Bool) -> Bool"),
        ("law.minimum", "num.min(a: Number, b: Number) -> Number"),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {declaration}\nkernel 1\ncharge 2\nnative\n"
        ))
        .unwrap();
        out.push((
            alias,
            registry
                .register(definition, Some(std::sync::Arc::new(Lookalike)))
                .unwrap(),
        ));
    }
    let references = ["num.add", "num.lt", "list.len"]
        .map(|name| (name, ids[name]))
        .into_iter()
        .chain(
            ["same", "bool.not", "num.min", "list.get"]
                .map(|name| (name, out.iter().find(|(key, _)| *key == name).unwrap().1)),
        )
        .map(|(name, id)| format!("use {name} = @{id}\n"))
        .collect::<String>();
    for (name, params, body) in [
        (
            "helper.scan",
            "values: List(Any), list: Bool",
            r#"
            let i = 0 let out = []
            while num.lt(i, list.len(values)) metered 2 {
                let present = false let value = absent
                if list { if num.lt(i, list.len(values)) {
                    set value = values[i] set present = bool.not(same(value, ()))
                } }
                if present { set out[list.len(out)] = value }
                set i = num.add(i, 1)
            }
            return out
        "#,
        ),
        (
            "helper.merge",
            "values: List(Any), compare: Any",
            r#"
            let sorted = values let out = [0, 0, 0, 0]
            let n = list.len(sorted) let width = 1
            while num.lt(width, n) metered 2 {
                let dest = 0 let end = 0 let mid = 0 let left = 0 let right = 0
                for unused in values metered 2 {
                    if same(dest, end) {
                        set mid = num.min(num.add(dest, width), n)
                        set end = num.min(num.add(mid, width), n)
                        set left = dest set right = mid
                    }
                    let from_right = false
                    if num.lt(left, mid) {
                        if num.lt(right, end) {
                            let first = list.get(sorted, left)
                            let second = list.get(sorted, right)
                            let output = apply compare(first, second)
                            let order = output
                            if num.lt(0, order) { set from_right = true }
                        }
                    } else { set from_right = true }
                    if from_right {
                        set out[dest] = list.get(sorted, right) set right = num.add(right, 1)
                    } else {
                        set out[dest] = list.get(sorted, left) set left = num.add(left, 1)
                    }
                    set dest = num.add(dest, 1)
                }
                set sorted = out set out = [0, 0, 0, 0] set width = num.add(width, width)
            }
            return sorted
        "#,
        ),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}({params}) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\n{references}body {{ {body} }}"
        )).unwrap();
        out.push((name, registry.register(definition, None).unwrap()));
        let (alternate, original, replacement) = match name {
            "helper.scan" => ("helper.scan_lookalike", "bool.not", "law.not"),
            "helper.merge" => ("helper.merge_lookalike", "num.min", "law.minimum"),
            _ => unreachable!(),
        };
        let current = out.iter().find(|(name, _)| *name == original).unwrap().1;
        let replacement = out.iter().find(|(name, _)| *name == replacement).unwrap().1;
        let references = references.replace(
            &format!("use {original} = @{current}"),
            &format!("use {original} = @{replacement}"),
        );
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {alternate}({params}) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\n{references}body {{ {body} }}"
        )).unwrap();
        out.push((alternate, registry.register(definition, None).unwrap()));
    }
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
    for (name, params, result, body) in [
        ("helper.typed", "x: Int", "Int", "return x"),
        ("helper.return_checked", "x: Any", "Int", "return x"),
        ("helper.optional", "x: Int, y?: Any", "Any", "return (x, y)"),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}({params}) -> {result}\nkernel 1\nmetering statements\nbody {{ {body} }}"
        )).unwrap();
        out.push((name, registry.register(definition, None).unwrap()));
    }
    let definition = lash_kernel_doc::parse_definition(
        "function helper.checked_answer(xs: List(Any), index: Int) -> Int\nkernel 1\ncharge 4\nbody { return xs[index] }",
    ).unwrap();
    let checked = registry.register(definition, None).unwrap();
    out.push(("helper.checked_answer", checked));
    let checked_plan = Box::leak(Box::new(lash_kernel_doc::plan::HelperPlan {
        name: "helper.checked_answer",
        function: checked,
        callees: &[],
        literals: &[],
        fields: &[],
        records: &[],
        registers: 2,
        slots: &[],
        closures: &[],
        sites: &[],
        run: |ctx, _| {
            ctx.begin()?;
            ctx.param(0, 0)?;
            ctx.param(1, 1)?;
            ctx.index(0)?;
            Ok(ctx.take(0))
        },
        template: Some(lash_kernel_doc::plan::HelperTemplate {
            entry: None,
            metered: None,
            depth: 1,
            callees: &[],
            heap: Some(lash_kernel_doc::plan::HeapTemplate {
                entry: Some(|heap, args, _| match args {
                    [target, key] => heap.index(*target, *key),
                    _ => None,
                }),
                callees: &[],
                depth: 1,
                writes: false,
                copy: None,
                element: None,
                record_write: None,
            }),
            forwards: Default::default(),
        }),
    }));
    registry.install_plans(&[checked_plan]).unwrap();
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
    // A heap-only partial helper models computed-key access: the record
    // arm is prepared; lists must retain their ordinary body in the loop.
    for (name, receiver) in [
        ("helper.element", "Any"),
        ("helper.element_list_any", "List(Any)"),
        ("helper.element_list_int", "List(Int)"),
        ("helper.element_map_any", "Map(Any, Any)"),
        ("helper.element_map_int", "Map(Text, Int)"),
    ] {
        let charge = if name == "helper.element_map_any" {
            "sum(4, nested(result))"
        } else {
            "4"
        };
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}(xs: {receiver}, index: Any) -> Any\nkernel 1\ncharge {charge}\nbody {{ try {{ return xs[index] }} catch failed {{ return absent }} }}",
        )).unwrap();
        let element = registry.register(definition, None).unwrap();
        let plan = Box::leak(Box::new(lash_kernel_doc::plan::HelperPlan {
            name,
            function: element,
            callees: &[],
            literals: &[],
            fields: &[],
            records: &[],
            registers: 2,
            slots: &[],
            closures: &[],
            sites: &[],
            run: |ctx, _| {
                ctx.begin()?;
                ctx.param(0, 0)?;
                ctx.param(1, 1)?;
                match ctx.index(0) {
                    Ok(()) => Ok(ctx.take(0)),
                    Err(lash_kernel_doc::plan::PlanExit::Raise(_)) => {
                        Ok(lash_kernel_doc::Value::Absent)
                    }
                    Err(stop) => Err(stop),
                }
            },
            template: Some(lash_kernel_doc::plan::HelperTemplate {
                entry: None,
                metered: None,
                depth: 1,
                callees: &[],
                heap: Some(lash_kernel_doc::plan::HeapTemplate {
                    entry: Some(if receiver == "Any" {
                        |heap, args, _| match args {
                            [record @ lash_kernel_doc::Value::Record(_), key] => {
                                heap.index(*record, *key)
                            }
                            _ => None,
                        }
                    } else {
                        |heap, args, _| match args {
                            [
                                target @ (lash_kernel_doc::Value::List(_)
                                | lash_kernel_doc::Value::Map(_)),
                                key,
                            ] => heap.index(*target, *key),
                            _ => None,
                        }
                    }),
                    callees: &[],
                    depth: 1,
                    writes: false,
                    copy: None,
                    element: (receiver == "Map(Any, Any)").then_some(
                        lash_kernel_doc::plan::ElementPath {
                            operation: lash_kernel_doc::plan::Element::ReadMap,
                            depth: 1,
                            room: 2,
                            requires: &[],
                        },
                    ),
                    record_write: None,
                }),
                forwards: Default::default(),
            }),
        }));
        registry.install_plans(&[plan]).unwrap();
        out.push((name, element));
    }
    for (name, ty) in [
        ("tuple.len", "Any"),
        ("map.len", "Map(Any, Any)"),
        ("set.len", "Set(Any)"),
        ("record.len", "Record{..Any}"),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}(xs: {ty}) -> Int\nkernel 1\ncharge 1\nnative\n"
        ))
        .unwrap();
        out.push((
            name,
            registry
                .register(definition, lash_kernel_lib::native(name))
                .unwrap(),
        ));
    }
    let definition = lash_kernel_doc::parse_definition(
        "function helper.minimum(a: Any, b: Any) -> Bool\nkernel 1\ncharge sum(7, min(size(a), size(b)))\nnative\n",
    ).unwrap();
    out.push((
        "helper.minimum",
        registry
            .register(definition, lash_kernel_lib::native("same"))
            .unwrap(),
    ));
    for (name, signature) in [
        ("text.utf16_compare", "(text: Text, other: Text) -> Int"),
        ("text.to_utf16_units", "(text: Text) -> List(Int)"),
        ("int.to_text", "(value: Int, base: Int) -> Text"),
        (
            "list.sort_by_keys",
            "(items: List(Any), keys: List(Any), descending: Bool) -> List(Any)",
        ),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}{signature}\nkernel 1\ncharge sum(1, size(result))\nnative\n"
        ))
        .unwrap();
        out.push((
            name,
            registry
                .register(definition, lash_kernel_lib::native(name))
                .unwrap(),
        ));
    }
    out
}

// FIG-7412: heap-only prepared computed access used to materialize every
// caller when its list arm declined. These are the primitive-key default
// sort and its adjacent-pair comparisons, including a park during keying.
const SORTS: &str = r#"
definition "function sort(items: Any, pause: Any) -> Any\nkernel 1\nmetering statements\nbody { let keys = [] let i = 0 while flat.lt(i, list.len(items)) { let item = invoke helper.element(items, i) let spelled = item if same(kind(item), \"integer\") { set spelled = int.to_text(item, 10) } set keys[list.len(keys)] = text.to_utf16_units(spelled) if pause { if same(i, 2) { do perform 0(spelled) as Any } } set i = flat.add(i, 1) } let ordered = list.sort_by_keys(items, keys, false) let j = 1 while flat.lt(j, list.len(ordered)) { let first = invoke helper.element(ordered, flat.add(j, -1)) let second = invoke helper.element(ordered, j) if same(kind(first), \"integer\") { set first = int.to_text(first, 10) } if same(kind(second), \"integer\") { set second = int.to_text(second, 10) } print text.utf16_compare(first, second) set j = flat.add(j, 1) } return ordered }"
main {
  let field = invoke helper.element({value: 7}, "value")
  let strings = call sort(["2", "10", "😀", "a", "", ""], false)
  let mixed = call sort([2, "10", "😀", 10, "2", ""], true)
  return (field, strings, mixed)
}
"#;

const REDUCTION: &str = r#"main {
    let values = [1, 2, 3, 4]
    let copied = invoke helper.copy(values)
    print invoke helper.place(values, [0, 0, 0, 0], 0)
    print invoke helper.place(values, [], 0)
    print invoke helper.place([], [], 0)
    let first = invoke helper.reduce(values)
    do perform 0(first) as Any
    set values[0] = 5
    print (first, invoke helper.reduce(values), invoke helper.reduce([]))
    print (copied, invoke helper.copy(values), invoke helper.copy([]))
    let overflow = null
    try { set overflow = invoke helper.reduce([1, 9223372036854775807]) } catch err { set overflow = err.kind }
    return overflow
}"#;

const VISIT: &str = "main { return (invoke helper.visit([1, (), 2, 3], 3), invoke helper.visit([1, (), 2, 3], 99)) }";

const SCAN: &str = r#"main {
    let values = [1, (), 2, 3]
    let first = invoke helper.scan(values, true)
    do perform 0(first) as Any
    set values[0] = 7
    return (first, invoke helper.scan(values, true), invoke helper.scan(values, false))
}"#;

const MERGE: &str = r#"main {
    let seen = [] let values = [{id: 0, k: 1}, {id: 1, k: 1}, {id: 2, k: 0}, {id: 3, k: 2}]
    let compare = fn(a, b) {
        set seen[list.len(seen)] = (a.id, b.id)
        if same(list.len(seen), 2) { do perform 0(seen) as Any }
        if num.lt(a.k, b.k) { return -1 } if num.lt(b.k, a.k) { return 1 } return 0
    }
    let ordered = invoke helper.merge(values, compare)
    let failing = fn(a, b) { return "mixed" }
    let failure = null
    try { do invoke helper.merge(values, failing) } catch err { set failure = err.kind }
    return (ordered, seen, values, failure)
}"#;

const LOOKALIKE_COUNTERS: &str = r#"main {
    let copied = invoke helper.scan_lookalike([1, (), 2, 3], true)
    let compare = fn(a, b) { if num.lt(a, b) { return -1 } if num.lt(b, a) { return 1 } return 0 }
    let merged = null
    try { set merged = invoke helper.merge_lookalike([3, 1, 2, 1], compare) } catch err { set merged = err.kind }
    return (copied, merged)
}"#;

/// K-CHG-007 / K-MACH-008: helper and callback frames preserve every
/// charge event and source continuation while entering the common loop.
#[test]
fn helper_frames_agree() {
    use super::ExecutionMode as Mode;
    use crate::machine::islands::witness::{self, Exit, Family};
    use crate::perf_witness::execution::{self, Owner};
    let collection_exits = std::cell::RefCell::new(witness::Counts::new());
    let limits = std::cell::Cell::new(super::super::embedder::ROOMY);
    let cold_slice = std::cell::Cell::new(7);
    let fixture = super::super::embedder::library_with(false, Some(helper_probes));
    let helpers = std::sync::Arc::new(
        lash_kernel_doc::BuildHelpers::new(
            (*fixture.registry).clone(),
            [
                "helper.typed",
                "helper.return_checked",
                "helper.optional",
                "helper.checked_answer",
                "helper.element_map_any",
            ]
            .map(|name| fixture.ids[name]),
        )
        .unwrap(),
    );
    let prepared =
        crate::PreparedLibrary::with_helpers(std::sync::Arc::clone(&fixture.registry), helpers);
    let run = |source: &str, layout, cold, taped, mode| {
        witness::begin();
        let mut embedder = Embedder::try_over(
            source,
            Setup {
                native_twice: false,
                probes: Some(helper_probes),
                layout,
                slice: if cold { cold_slice.get() } else { u64::MAX },
                bounds: limits.get(),
                ..Setup::default()
            },
            Some(prepared.clone()),
        )
        .unwrap();
        if source == SORTS && layout == Layout(1) {
            embedder.machine.count_work();
        }
        let tape = crate::ChargeTape::new();
        if taped {
            embedder.machine.record_charges(tape.clone());
        }
        if ![REDUCTION, VISIT, SCAN, MERGE, LOOKALIKE_COUNTERS].contains(&source) {
            embedder.machine.count_execution();
        }
        let mut checkpoints = Vec::new();
        let mut stops = Vec::new();
        let end = loop {
            match mode {
                Mode::Source => embedder.machine.force_source_execution(),
                Mode::Generic => embedder.machine.force_generic_packets(),
                Mode::Specialized => {}
            }
            let step = embedder.try_run().unwrap_or_else(|error| {
                panic!("helper failed for layout {layout:?}, mode {mode:?}, cold={cold}, taped={taped}, charge={:?}: {error:?}; {source}", embedder.machine.meters().charged)
            });
            stops.push((
                match &step {
                    Step::Slice => "slice",
                    Step::Parked(_) => "parked",
                    Step::Ended(_) => "ended",
                },
                embedder.machine.meters(),
                crate::testing::owner_balance(&embedder.machine).unwrap(),
            ));
            match step {
                Step::Ended(end) => {
                    if limits.get() == super::super::embedder::ROOMY {
                        assert!(matches!(end, End::Finished(_)), "{end:?}");
                    }
                    break match end {
                        end @ End::Finished(_) => format!("{:?}", result(end)),
                        other => format!("{other:?}"),
                    };
                }
                Step::Parked(_) => {
                    checkpoints.push(embedder.machine.semantic_checkpoint());
                    if cold {
                        embedder.reopen(layout, true);
                        if source == SORTS && layout == Layout(1) {
                            embedder.machine.count_work();
                        }
                        if taped {
                            embedder.machine.record_charges(tape.clone());
                        }
                    }
                    embedder.deliver_next();
                }
                Step::Slice => {
                    if cold {
                        checkpoints.push(embedder.machine.semantic_checkpoint());
                        embedder.reopen(layout, true);
                        if source == SORTS && layout == Layout(1) {
                            embedder.machine.count_work();
                        }
                        if taped {
                            embedder.machine.record_charges(tape.clone());
                        }
                    }
                }
            }
        };
        let exits = witness::take();
        if mode == Mode::Specialized && layout == Layout(0) && !taped {
            let mut counts = collection_exits.borrow_mut();
            for (event, count) in exits {
                *counts.entry(event).or_default() += count;
            }
        } else if mode != Mode::Specialized {
            assert!(
                exits.is_empty(),
                "a forced path selected a collection family: {exits:?}"
            );
        }
        let census = embedder.machine.take_execution_census();
        (
            (
                end,
                embedder.world.printed,
                embedder.requests,
                embedder.machine.meters().charged,
                lash_kernel_exec::Weight::expand(tape.steps()),
                checkpoints,
                stops,
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
    const CONTRACTS: [&str; 16] = [
        REDUCTION,
        VISIT,
        SCAN,
        MERGE,
        LOOKALIKE_COUNTERS,
        r#"main { let data = map{"value": [1, 2]} let before = helper.minimum(data, absent) do perform 0(before) as Any return (before, helper.minimum(data, absent), helper.minimum("x", null)) }"#,
        r#"main { let xs = [1] let pair = (1, 2) let entries = map{"a": 1} let members = set{1, 2} let record = {a: 1} let before = (list.len(xs), tuple.len(pair), map.len(entries), set.len(members), record.len(record)) do perform 0(before) as Any set xs[1] = 2 set entries["b"] = 2 try { print map.len(xs) } catch err { print err.kind } return (before, list.len(xs), map.len(entries), map.len(map{}), set.len(set{}), tuple.len(()), record.len({})) }"#,
        r#"main { let xs = [1, 2, 3] let observe = fn() { return xs } remove xs[2] do perform 0(apply observe()) as Any remove xs[1] remove xs[0] return apply observe() }"#,
        r#"main { let xs = [1, 2, 3] remove xs[0] try { remove xs[9] } catch err { return (xs, err.kind) } return xs }"#,
        r#"main { let xs = map{"value": 1} let first = invoke helper.element_map_any(xs, "value") do perform 0(first) as Any set xs["value"] = 2 let second = invoke helper.element_map_any(xs, "value") let missing = invoke helper.element_map_any(xs, "missing") return (first, second, missing) }"#,
        r#"main { let xs = map{"value": (1, [2])} let found = invoke helper.element_map_any(xs, "value") do perform 0(found) as Any return found }"#,
        r#"main { try { return invoke helper.checked_answer(["bad"], 0) } catch err { return err.kind } }"#,
        "main { return invoke helper.typed(7) }",
        "main { return invoke helper.optional(7) }",
        "main { try { return invoke helper.typed(false) } catch err { return err.kind } }",
        "main { try { return invoke helper.return_checked(\"bad\") } catch err { return err.kind } }",
    ];
    for source in [REDUCTION, VISIT, SCAN, MERGE, LOOKALIKE_COUNTERS] {
        for cold in [false, true] {
            let reference = run(source, Layout(0), cold, false, Mode::Source).0;
            for mode in [Mode::Generic, Mode::Specialized] {
                assert_eq!(
                    run(source, Layout(0), cold, false, mode).0,
                    reference,
                    "quiet collection {mode:?}, cold={cold}: {source}"
                );
            }
        }
    }
    for source in CONTRACTS.into_iter().chain([
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
        SORTS,
    ]) {
        for cold in [false, true] {
            crate::machine::islands::COLLECTION_VISITS.with(|n| n.set(0));
            let (actual, census) = run(source, Layout(0), cold, true, Mode::Specialized);
            if source == VISIT && !cold {
                assert!(
                    crate::machine::islands::COLLECTION_VISITS.with(|n| n.get()) > 0,
                    "K-ITER-005: the visitor must execute both relocated arm entries"
                );
                assert_eq!(
                    actual.0,
                    format!(
                        "{:?}",
                        Datum::Tuple(vec![
                            Datum::Tuple(vec![Datum::List(vec![int(1)]), int(3)]),
                            Datum::Tuple(vec![Datum::List(vec![int(1), int(2), int(3)]), int(5)]),
                        ])
                    )
                );
            }
            if source == SORTS || CONTRACTS.contains(&source) {
                let reference = run(
                    source,
                    if source == SORTS {
                        Layout(1)
                    } else {
                        Layout(3)
                    },
                    cold,
                    true,
                    Mode::Specialized,
                )
                .0;
                assert_eq!(actual.0, reference.0, "helper result: {source}");
                assert_eq!(actual.1, reference.1, "helper observations: {source}");
                assert_eq!(actual.2, reference.2, "helper requests: {source}");
                assert_eq!(actual.3, reference.3, "helper charge: {source}");
                assert_eq!(actual.4, reference.4, "helper events: {source}");
                assert_eq!(actual.5, reference.5, "helper checkpoints: {source}");
                assert_eq!(
                    actual.6, reference.6,
                    "helper stops and held bytes: {source}"
                );
            }
            if source.contains("helper.checked_answer(") {
                assert_eq!(actual.0, format!("{:?}", text("entry_type_mismatch")));
            }
            if source == SORTS {
                assert_eq!(
                    actual.0,
                    format!(
                        "{:?}",
                        Datum::Tuple(vec![
                            int(7),
                            Datum::List(["", "10", "2", "a", "😀", ""].map(text).to_vec()),
                            Datum::List(vec![
                                text("10"),
                                int(10),
                                int(2),
                                text("2"),
                                text("😀"),
                                text("")
                            ])
                        ])
                    )
                );
            }
            if source == REDUCTION {
                assert_eq!(
                    actual.1,
                    vec![
                        Datum::Tuple(vec![
                            Datum::List(vec![int(1), int(2), int(3), int(4)]),
                            int(4),
                        ]),
                        Datum::Tuple(vec![
                            Datum::List(vec![int(1), int(2), int(3), int(4)]),
                            int(4),
                        ]),
                        Datum::Tuple(vec![Datum::List(vec![]), int(0)]),
                        Datum::Tuple(vec![int(10), int(14), int(0)]),
                        Datum::Tuple(vec![
                            Datum::List(vec![int(1), int(2), int(3), int(4)]),
                            Datum::List(vec![int(5), int(2), int(3), int(4)]),
                            Datum::List(vec![]),
                        ]),
                    ]
                );
            }
            if !cold && ![REDUCTION, VISIT, SCAN, MERGE, LOOKALIKE_COUNTERS].contains(&source) {
                if source == SORTS {
                    assert!(
                        census
                            .operation_exits
                            .values()
                            .all(|ops| !ops.contains_key("helper.element")),
                        "computed sort reads must stay in the register program: {census:?}"
                    );
                }
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
            }
        }
    }
    for source in [SCAN, MERGE, LOOKALIKE_COUNTERS] {
        for slice in [1, 31] {
            cold_slice.set(slice);
            let source_only = run(source, Layout(0), true, false, Mode::Source).0;
            for mode in [Mode::Generic, Mode::Specialized] {
                let actual = run(source, Layout(0), true, false, mode).0;
                assert_eq!(
                    (
                        &actual.0, &actual.1, &actual.2, &actual.3, &actual.4, &actual.5
                    ),
                    (
                        &source_only.0,
                        &source_only.1,
                        &source_only.2,
                        &source_only.3,
                        &source_only.4,
                        &source_only.5
                    ),
                    "collection cold semantics at slice {slice}, {mode:?}: {source}"
                );
                assert_eq!(
                    actual.6, source_only.6,
                    "collection cold memory at slice {slice}, {mode:?}: {source}"
                );
            }
        }
    }
    let counts = collection_exits.borrow();
    for family in [
        Family::CollectionAppend,
        Family::Reduction,
        Family::Placement,
        Family::Visit,
        Family::ReadSlot,
        Family::Merge,
    ] {
        assert!(
            counts.get(&(family, Exit::Commit)).copied().unwrap_or(0) > 0,
            "the quiet helper must commit {family:?}: {counts:?}"
        );
        assert!(
            counts.get(&(family, Exit::Guard)).copied().unwrap_or(0) > 0,
            "the helper must reach {family:?}'s generic suffix: {counts:?}"
        );
    }
    assert!(
        counts
            .get(&(Family::Merge, Exit::Callback))
            .copied()
            .unwrap_or(0)
            > 0,
        "the merge must publish its prefix before the actual guest callback: {counts:?}"
    );
    println!("quiet collection family exits: {counts:?}");
    drop(counts);
    for source in [SCAN, MERGE, LOOKALIKE_COUNTERS] {
        let reference = run(source, Layout(0), false, false, Mode::Source).0;
        for charge_limit in 0..=reference.3.expect("exact charge is enabled") {
            limits.set(Bounds {
                charge_limit,
                ..super::super::embedder::ROOMY
            });
            let source_only = run(source, Layout(0), false, false, Mode::Source).0;
            for mode in [Mode::Generic, Mode::Specialized] {
                assert_eq!(
                    run(source, Layout(0), false, false, mode).0,
                    source_only,
                    "collection refusal {charge_limit}, {mode:?}: {source}"
                );
            }
        }
        limits.set(super::super::embedder::ROOMY);
    }
}

/// A native's display name does not certify its collection semantics.
#[test]
fn collection_length_admission_uses_native_evidence() {
    struct Lookalike;
    impl lash_kernel_doc::NativeFunction for Lookalike {
        fn call(
            &self,
            _: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            Ok(lash_kernel_doc::Value::Int(17))
        }
    }
    fn probe(
        registry: &mut lash_kernel_doc::FunctionRegistry,
        _: &std::collections::BTreeMap<&'static str, lash_kernel_value::FunctionId>,
    ) -> Vec<(&'static str, lash_kernel_value::FunctionId)> {
        let definition = lash_kernel_doc::parse_definition(
            "function map.len(xs: Any) -> Int\nkernel 1\ncharge 1\nnative\n",
        )
        .unwrap();
        vec![(
            "map.len",
            registry
                .register(definition, Some(std::sync::Arc::new(Lookalike)))
                .unwrap(),
        )]
    }
    for layout in [Layout(0), Layout(3)] {
        let mut embedder = Embedder::with(
            "main { return map.len(map{}) }",
            Setup {
                probes: Some(probe),
                layout,
                ..Setup::default()
            },
        );
        let Step::Ended(end) = embedder.run() else {
            panic!("a constant native finishes")
        };
        assert_eq!(result(end), int(17), "{layout:?}");
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
    let actual = run(Layout(0));
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
