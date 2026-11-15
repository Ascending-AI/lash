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
    struct BoundaryNative;
    impl lash_kernel_doc::NativeFunction for BoundaryNative {
        fn prepare(&self) -> Option<lash_kernel_doc::PreparedNative> {
            Some(lash_kernel_doc::PreparedNative {
                pair: None,
                entry: |args| match args {
                    [lash_kernel_doc::Value::Int(0 | 2 | 6)] => {
                        Some(lash_kernel_doc::Value::Int(7))
                    }
                    _ => None,
                },
                charged_entry: None,
                operation: "law.boundary",
            })
        }
        fn call(
            &self,
            call: lash_kernel_doc::NativeCall<'_, '_>,
        ) -> Result<lash_kernel_doc::Value, lash_kernel_doc::NativeError> {
            use lash_kernel_doc::{NativeError, NewObject, Value};
            match call.args.values() {
                [Value::Int(0 | 2)] => Ok(Value::Int(7)),
                [Value::Int(6)] => {
                    call.counter.spend(2)?;
                    Ok(Value::Int(7))
                }
                [Value::Int(1 | 3)] | [Value::Absent] => {
                    call.counter.spend(2)?;
                    Ok(call
                        .new
                        .allocate(NewObject::List(vec![Value::Int(7), Value::Int(8)])))
                }
                [Value::Int(4)] => panic!("unknown contracts must decline before native execution"),
                _ => Err(NativeError::Refused {
                    kind: "boundary_refused",
                    message: "native refusal".into(),
                }),
            }
        }
        fn resource_contract(
            &self,
            args: lash_kernel_doc::Args<'_>,
        ) -> Result<Option<lash_kernel_doc::NativeResourceContract>, lash_kernel_doc::NativeError>
        {
            use lash_kernel_doc::{
                NativeResourceContract as Contract, NativeResourceEntity as Kind,
                NativeResourceEvent as Event, NativeResourceExit as Exit,
                NativeResourceHolding as Holding, NativeResourceOrigin as Origin,
                NativeResourceOwner as Owner, NativeResourceRelease as Release, Value,
            };
            let mode = match args.values() {
                [Value::Int(mode)] => *mode,
                [Value::Absent] => 1,
                _ => return Ok(None),
            };
            Ok(match mode {
                0 | 5 => Some(Contract::value(Exit::Returned, 17, Origin::Immutable)),
                6 => Some(Contract {
                    exit: Exit::Returned,
                    events: vec![Event::Scratch {
                        owner: Owner(0),
                        bytes: 40,
                        release: Release::Exit,
                    }],
                    output: Holding {
                        bytes: 17,
                        origin: Origin::Immutable,
                    },
                }),
                2 => Some(Contract::value(Exit::Returned, 16, Origin::Immutable)),
                1 | 3 => Some(Contract {
                    exit: Exit::Returned,
                    events: vec![
                        Event::Scratch {
                            owner: Owner(0),
                            bytes: 40,
                            release: Release::Before(Owner(1)),
                        },
                        Event::Scratch {
                            owner: Owner(1),
                            bytes: 20,
                            release: Release::Exit,
                        },
                        Event::Scratch {
                            owner: Owner(2),
                            bytes: 30,
                            release: Release::Before(Owner(3)),
                        },
                        Event::Entity {
                            owner: Owner(3),
                            kind: Kind::List,
                            release: None,
                            members: vec![
                                Holding {
                                    bytes: 17,
                                    origin: Origin::Immutable,
                                },
                                Holding {
                                    bytes: if mode == 3 { 16 } else { 17 },
                                    origin: Origin::Immutable,
                                },
                            ],
                        },
                    ],
                    output: Holding {
                        bytes: 16,
                        origin: Origin::Fresh(Owner(3)),
                    },
                }),
                _ => None,
            })
        }
    }
    let boundary = lash_kernel_doc::parse_definition(
        "function resident.boundary(value: Int) -> Any\nkernel 1\ncharge sum(1, size(value), size(result))\nnative\n",
    ).unwrap();
    out.push((
        "resident.boundary",
        registry
            .register(boundary, Some(std::sync::Arc::new(BoundaryNative)))
            .unwrap(),
    ));

    let optional = lash_kernel_doc::parse_definition(
        "function resident.optional(value?: Int) -> Any\nkernel 1\ncharge sum(1, size(value), size(result))\nnative\n",
    ).unwrap();
    out.push((
        "resident.optional",
        registry
            .register(optional, Some(std::sync::Arc::new(BoundaryNative)))
            .unwrap(),
    ));

    for (definition, native) in lash_kernel_lib::text_json() {
        let name = match definition.name.as_str() {
            "text.join" => "text.join",
            "text.repeat" => "text.repeat",
            "text.slice" => "text.slice",
            "json.decode" => "json.decode",
            "json.stringify" => "json.stringify",
            "json.render" => "json.render",
            _ => continue,
        };
        out.push((name, registry.register(definition, Some(native)).unwrap()));
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
    for (name, native, signature) in [
        (
            "resident.minimum",
            "num.min",
            "a: Number, b: Number) -> Number",
        ),
        ("resident.add", "num.add", "a: Number, b: Number) -> Number"),
        (
            "resident.large_add",
            "num.add",
            "a: Number, b: Number) -> Number",
        ),
        ("resident.negate", "num.neg", "value: Number) -> Number"),
    ] {
        let charge = if name == "resident.large_add" {
            "sum(1, size(a), size(b), size(result))"
        } else {
            "1"
        };
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}({signature}\nkernel 1\ncharge {charge}\nnative\n"
        ))
        .unwrap();
        out.push((
            name,
            registry
                .register(definition, lash_kernel_lib::native(native))
                .unwrap(),
        ));
    }
    // Use the shipped length contract without changing the deliberately
    // uncertified Len native shared by the older fixtures.
    let length = lash_kernel_doc::parse_definition(
        "function resident.len(values: List(Any)) -> Int\nkernel 1\ncharge 1\nnative\n",
    )
    .unwrap();
    out.push((
        "resident.len",
        registry
            .register(length, lash_kernel_lib::native("list.len"))
            .unwrap(),
    ));
    let copy = lash_kernel_doc::parse_definition(
        "function resident.copy(xs: List(Any)) -> List(Any)\nkernel 1\ncharge sum(1, size(xs), size(result))\nnative\n",
    ).unwrap();
    out.push((
        "resident.copy",
        registry
            .register(copy, lash_kernel_lib::native("list.copy"))
            .unwrap(),
    ));
    for (name, native, ty) in [
        ("resident.copy_map", "map.copy", "Map(Any, Any)"),
        ("resident.copy_set", "set.copy", "Set(Any)"),
        ("resident.copy_record", "record.copy", "Record{..Any}"),
        ("resident.deep_copy", "list.copy_deep", "List(Any)"),
    ] {
        let definition = lash_kernel_doc::parse_definition(&format!(
            "function {name}(xs: {ty}) -> {ty}\nkernel 1\ncharge sum(1, size(xs), size(result))\nnative\n"
        )).unwrap();
        out.push((
            name,
            registry
                .register(definition, lash_kernel_lib::native(native))
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
    // Collection-prefix witnesses need the installed resource contract. The
    // deliberately uncertified Len also pins a real publication refusal.
    let length = out
        .iter()
        .find(|(name, _)| *name == "resident.len")
        .unwrap()
        .1;
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.copy(values: List(Any)) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\nuse resident.len = @{length}\nbody {{ let out = [] for value in values metered 2 {{ set out[resident.len(out)] = value }} return out }}"
    )).unwrap();
    out.push(("helper.copy", registry.register(definition, None).unwrap()));
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.copy_uncertified(values: List(Any)) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\nuse list.len = @{}\nbody {{ let out = [] for value in values metered 2 {{ set out[list.len(out)] = value }} return out }}", ids["list.len"]
    )).unwrap();
    out.push((
        "helper.copy_uncertified",
        registry.register(definition, None).unwrap(),
    ));
    let addition = add;
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.place(values: List(Any), target: List(Any), position: Int) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\nuse flat.add = @{addition}\nbody {{ for value in values metered 2 {{ set target[position] = value set position = flat.add(position, 1) }} return (target, position) }}"
    )).unwrap();
    out.push(("helper.place", registry.register(definition, None).unwrap()));
    let visit_length = ids["list.len"];
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function helper.visit(values: List(Any), bound: Int) -> Any\nkernel 1\ncharge sum(4, deep(values), size(result))\nuse same = @{same}\nuse flat.add = @{add}\nuse list.len = @{visit_length}\nbody {{ let out = [] let i = 0 for value in values metered 2 {{ if same(i, bound) {{ break }} if same(value, ()) {{ set i = flat.add(i, 1) }} else {{ set out[list.len(out)] = value }} set i = flat.add(i, 1) }} return (out, i) }}"
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
    let references = [
        ("num.add", ids["num.add"]),
        ("num.lt", ids["num.lt"]),
        ("resident.len", length),
    ]
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
            while num.lt(i, resident.len(values)) metered 2 {
                let present = false let value = absent
                if list { if num.lt(i, resident.len(values)) {
                    set value = values[i] set present = bool.not(same(value, ()))
                } }
                if present { set out[resident.len(out)] = value }
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
            let n = resident.len(sorted) let width = 1
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
        (
            "helper.typed_members",
            "xs: List(Int), m: Map(Text, Any), s: Set(Any)",
            "Any",
            "return 0",
        ),
        ("helper.optional", "x: Int, y?: Any", "Any", "return (x, y)"),
        (
            "helper.apply_typed",
            "f: Fn(x: Int) -> Int",
            "Any",
            "return apply f(1)",
        ),
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
    let uncertified = invoke helper.copy_uncertified([1, (), 2, 3])
    let copied = invoke helper.scan_lookalike([1, (), 2, 3], true)
    let compare = fn(a, b) { if num.lt(a, b) { return -1 } if num.lt(b, a) { return 1 } return 0 }
    let merged = null
    try { set merged = invoke helper.merge_lookalike([3, 1, 2, 1], compare) } catch err { set merged = err.kind }
    return (copied, merged, uncertified)
}"#;

/// K-CHG-007 / K-MACH-008 / K-LIB-006: helper and callback frames preserve
/// every charge event and source continuation, with each native or the body
/// it implements, warm or sealed cold.
#[test]
fn helper_frames_agree() {
    let limits = std::cell::Cell::new(super::super::embedder::ROOMY);
    let cold_slice = std::cell::Cell::new(7);
    let fixture = super::super::embedder::library_with(false, Some(helper_probes));
    let helpers = std::sync::Arc::new(
        lash_kernel_doc::BuildHelpers::new(
            (*fixture.registry).clone(),
            [
                "helper.typed",
                "helper.return_checked",
                "helper.typed_members",
                "helper.optional",
                "helper.apply_typed",
                "helper.checked_answer",
                "helper.element_map_any",
            ]
            .map(|name| fixture.ids[name]),
        )
        .unwrap(),
    );
    let prepared =
        crate::PreparedLibrary::with_helpers(std::sync::Arc::clone(&fixture.registry), helpers);
    let bodies = crate::testing::bodies(&prepared);
    let run = |source: &str, body: bool, cold, taped| {
        let mut embedder = Embedder::try_over(
            source,
            Setup {
                native_twice: false,
                probes: Some(helper_probes),
                slice: if cold { cold_slice.get() } else { u64::MAX },
                bounds: limits.get(),
                ..Setup::default()
            },
            Some(if body {
                bodies.clone()
            } else {
                prepared.clone()
            }),
        )
        .unwrap();
        let tape = crate::ChargeTape::new();
        if taped {
            embedder.machine.record_charges(tape.clone());
        }

        let mut checkpoints = Vec::new();
        let mut stops = Vec::new();
        let end = loop {
            let step = embedder.try_run().unwrap_or_else(|error| {
                panic!("helper failed with bodies={body}, cold={cold}, taped={taped}, charge={:?}: {error:?}; {source}", embedder.machine.meters().charged)
            });
            stops.push((
                match &step {
                    Step::Slice => "slice",
                    Step::Parked(_) => "parked",
                    Step::Ended(_) => "ended",
                },
                embedder.machine.meters(),
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
                        embedder.reopen(true);
                        if taped {
                            embedder.machine.record_charges(tape.clone());
                        }
                    }
                    embedder.deliver_next();
                }
                Step::Slice => {
                    if cold {
                        checkpoints.push(embedder.machine.semantic_checkpoint());
                        embedder.reopen(true);
                        if taped {
                            embedder.machine.record_charges(tape.clone());
                        }
                    }
                }
            }
        };

        (
            end,
            embedder.world.printed,
            embedder.requests,
            embedder.machine.meters().charged,
            crate::Weight::expand(tape.steps()),
            checkpoints,
            stops,
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
    const CONTRACTS: [&str; 18] = [
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
        // A collection argument fits by its kind alone only when its
        // members may be of any type: a member or key of the wrong type
        // does not fit.
        r#"main { for args in [([1], map{"a": "x"}, set{1}), (["bad"], map{}, set{}), ([], map{1: 2}, set{})] { try { print invoke helper.typed_members(args[0], args[1], args[2]) } catch err { print err.kind } } return 0 }"#,
        "main { try { return invoke helper.return_checked(\"bad\") } catch err { return err.kind } }",
        "main { try { return invoke helper.apply_typed(fn(a, b) { return a }) } catch err { return err.kind } }",
    ];
    // What a run shows its embedder whatever its slices: result, prints,
    // effects, charge and the charge events in order.
    type Run = (
        String,
        Vec<Datum>,
        Vec<Request>,
        Option<u64>,
        Vec<crate::Weight>,
        Vec<lash_kernel_state::semantic::Checkpoint>,
        Vec<(&'static str, crate::Meters)>,
    );
    let outcome = |run: &Run| {
        (
            run.0.clone(),
            run.1.clone(),
            run.2.clone(),
            run.3,
            run.4.clone(),
        )
    };
    // The fixture's native raises its declared error before the body it
    // implements could run: a native may refuse where its body would not.
    const NATIVE_REFUSES: &str =
        "main { try { return helper.native(-1) } catch err { return err.kind } }";
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
        NATIVE_REFUSES,
        BODY_PARK,
        CHARGED_ROOT,
        HELPERS,
        SORTS,
    ]) {
        let warm = run(source, false, false, true);
        for cold in [false, true] {
            let actual = run(source, false, cold, true);
            assert_eq!(outcome(&actual), outcome(&warm), "cold={cold}: {source}");
            if source == VISIT && !cold {
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
            if source != NATIVE_REFUSES {
                let reference = run(source, true, cold, true);
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
            // A typed helper body's caller relies on its declared result,
            // and the body on a function argument's declared signature.
            if source.contains("helper.return_checked(") {
                assert_eq!(actual.0, format!("{:?}", text("entry_type_mismatch")));
            }
            if source.contains("helper.typed_members(") {
                let refused = text("function_argument");
                assert_eq!(actual.1, [int(0), refused.clone(), refused]);
            }
            if source.contains("helper.apply_typed(") {
                assert_eq!(actual.0, format!("{:?}", text("function_argument")));
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
        }
    }
    for source in [REDUCTION, VISIT, SCAN, MERGE, LOOKALIKE_COUNTERS] {
        let warm = outcome(&run(source, false, false, false));
        for slice in [1, 7, 31] {
            cold_slice.set(slice);
            assert_eq!(
                outcome(&run(source, false, true, false)),
                warm,
                "collection cold semantics at slice {slice}: {source}"
            );
        }
        cold_slice.set(7);
    }

    for source in [SCAN, MERGE, LOOKALIKE_COUNTERS] {
        let reference = run(source, false, false, false);
        for charge_limit in 0..=reference.3.expect("exact charge is enabled") {
            limits.set(Bounds {
                charge_limit,
                ..super::super::embedder::ROOMY
            });
            assert_eq!(
                outcome(&run(source, false, true, false)),
                outcome(&run(source, false, false, false)),
                "cold collection refusal {charge_limit}: {source}"
            );
        }
        limits.set(super::super::embedder::ROOMY);
    }
    native_boundaries_agree();
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
    let mut embedder = Embedder::with(
        "main { return map.len(map{}) }",
        Setup {
            probes: Some(probe),
            ..Setup::default()
        },
    );
    let Step::Ended(end) = embedder.run() else {
        panic!("a constant native finishes")
    };
    assert_eq!(result(end), int(17));
}

/// Native exact acceptance binds real operands and preserves paid closes,
/// optional padding, graph/scratch peaks and durable cold source handoffs:
/// sealed and reopened cold, a run ends and refuses as it does warm,
/// holding no more (contract §2).
fn native_boundaries_agree() {
    const SOURCE: &str = "main { let xs = resident.optional() let ys = resident.boundary(1) let n = list.len(xs) let value = resident.boundary(0) return (value, n, list.get(ys, 0)) }";
    let observe = |source: &str, cold, taped, bounds| {
        let mut run = Embedder::with(
            source,
            Setup {
                probes: Some(helper_probes),
                bounds,
                slice: if cold { 31 } else { u64::MAX },
                ..Setup::default()
            },
        );
        let tape = crate::ChargeTape::new();
        if taped {
            run.machine.record_charges(tape.clone());
        }

        let mut boundaries = Vec::new();
        let end = loop {
            let step = run.run();

            if matches!(step, Step::Parked(_)) {
                boundaries.push((
                    run.machine.semantic_checkpoint(),
                    run.machine.meters().memory,
                ));
            }
            match step {
                Step::Ended(end) => break end,
                Step::Slice => {
                    if cold {
                        run.reopen(true);
                        if taped {
                            run.machine.record_charges(tape.clone());
                        }
                    }
                }
                Step::Parked(_) => {
                    assert_ne!(source, SOURCE, "native boundary fixture cannot park");
                    if cold {
                        run.reopen(true);
                        if taped {
                            run.machine.record_charges(tape.clone());
                        }
                    }
                    run.deliver_next();
                }
            }
        };
        (
            end,
            run.machine.meters().charged,
            crate::Weight::expand(tape.steps()),
            boundaries,
            run.requests,
            run.machine.meters().memory,
        )
    };
    let roomy = super::super::embedder::ROOMY;
    let reference = observe(SOURCE, false, false, roomy);
    assert_eq!(
        result(reference.0.clone()),
        Datum::Tuple(vec![int(7), int(2), int(7)])
    );
    super::native_codecs::cold_continues(&observe(SOURCE, true, false, roomy), &reference, "cold");
    super::native_codecs::cold_continues(
        &observe(SOURCE, true, true, roomy),
        &observe(SOURCE, false, true, roomy),
        "native canonical cold charge tape",
    );
    let exact_charge = reference.1.unwrap();
    for charge_limit in [0, 3, 7, exact_charge - 1, exact_charge] {
        let bounds = Bounds {
            charge_limit,
            ..roomy
        };
        super::native_codecs::cold_continues(
            &observe(SOURCE, true, true, bounds),
            &observe(SOURCE, false, true, bounds),
            &format!("native first charge refusal at {charge_limit}"),
        );
    }
    for memory in [128, 229, 230, 256, 512] {
        let bounds = Bounds { memory, ..roomy };
        super::native_codecs::cold_continues(
            &observe(SOURCE, true, true, bounds),
            &observe(SOURCE, false, true, bounds),
            &format!("native normalized first memory refusal at {memory}"),
        );
    }

    super::native_codecs::agree(&observe);
}
