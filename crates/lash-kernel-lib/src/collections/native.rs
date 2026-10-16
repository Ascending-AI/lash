use std::ops::ControlFlow;
use std::sync::Arc;

use lash_kernel_doc::{
    Element, EntityId, Float, Identity, Integer, NativeCall, NativeError, NativeFunction,
    NativeHeap, Object, PreparedNative, Value,
};
use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive};

use super::{CollectionError, copy, raise};
use crate::numbers::KIND_NAMES;
use lash_kernel_doc::heap::{
    ErrorRef, FieldName, HeapFault, IntRef, LogicalBytes, Payload, TextRef,
};

type NativeDefinition<T> = (T, Arc<dyn NativeFunction>);

#[derive(Clone, Copy)]
enum Kind {
    List,
    Tuple,
    Map,
    Set,
    Record,
}
#[derive(Clone, Copy)]
enum Op {
    Len,
    Get,
    GetOr,
    At,
    InsertIndex,
    Slice,
    Concat,
    Contains,
    IndexOf,
    Keys,
    Values,
    Entries,
    Copy,
    DeepCopy,
    With,
    Without,
    Check,
}
struct Native {
    kind: Kind,
    op: Op,
}

pub(super) fn functions<T>(
    mut define: impl FnMut(&str, &dyn Fn() -> String) -> Result<T, CollectionError>,
) -> Result<Vec<NativeDefinition<T>>, CollectionError> {
    let mut functions = Vec::new();
    let kinds = [
        (Kind::List, "list", "List(Any)"),
        (Kind::Tuple, "tuple", "Any"),
        (Kind::Map, "map", "Map(Any, Any)"),
        (Kind::Set, "set", "Set(Any)"),
        (Kind::Record, "record", "Record{..Any}"),
    ];
    for (kind, prefix, ty) in kinds {
        let mut ops = vec![
            ("len", Op::Len, "", "Int", "1"),
            ("copy", Op::Copy, "", ty, "sum(1, size(xs), size(result))"),
            (
                "copy_deep",
                Op::DeepCopy,
                "",
                ty,
                "sum(1, deep(xs), deep(result))",
            ),
            ("check", Op::Check, "", ty, "1"),
        ];
        match kind {
            Kind::List | Kind::Tuple => ops.extend([
                (
                    "get",
                    Op::Get,
                    ", index: Number",
                    "Any",
                    "sum(1, size(index))",
                ),
                (
                    "at",
                    Op::At,
                    ", index: Number",
                    "Any",
                    "sum(1, size(index))",
                ),
                (
                    "insert_index",
                    Op::InsertIndex,
                    ", index: Number",
                    "Int",
                    "sum(1, size(index))",
                ),
                (
                    "slice",
                    Op::Slice,
                    ", start: Number, end?: Number",
                    ty,
                    "sum(1, size(xs), size(start), size(end), size(result))",
                ),
                (
                    "concat",
                    Op::Concat,
                    ", other: Any",
                    ty,
                    "sum(1, size(xs), size(other), size(result))",
                ),
                (
                    "contains",
                    Op::Contains,
                    ", value: Any",
                    "Bool",
                    "sum(1, deep(xs), product(size(xs), deep(value)))",
                ),
                (
                    "index_of",
                    Op::IndexOf,
                    ", value: Any",
                    "Int",
                    "sum(1, deep(xs), product(size(xs), deep(value)))",
                ),
                (
                    "keys",
                    Op::Keys,
                    "",
                    "List(Int)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "values",
                    Op::Values,
                    "",
                    "List(Any)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "entries",
                    Op::Entries,
                    "",
                    "List(Tuple(Int, Any))",
                    "sum(1, size(xs), size(result))",
                ),
            ]),
            Kind::Map => ops.extend([
                (
                    "get",
                    Op::Get,
                    ", key: Any",
                    "Any",
                    "sum(1, size(xs), deep(key))",
                ),
                (
                    "contains",
                    Op::Contains,
                    ", key: Any",
                    "Bool",
                    "sum(1, size(xs), deep(key))",
                ),
                (
                    "keys",
                    Op::Keys,
                    "",
                    "List(Any)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "values",
                    Op::Values,
                    "",
                    "List(Any)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "entries",
                    Op::Entries,
                    "",
                    "List(Tuple(Any, Any))",
                    "sum(1, size(xs), size(result))",
                ),
            ]),
            Kind::Set => ops.extend([
                (
                    "contains",
                    Op::Contains,
                    ", key: Any",
                    "Bool",
                    "sum(1, size(xs), deep(key))",
                ),
                (
                    "keys",
                    Op::Keys,
                    "",
                    "List(Any)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "values",
                    Op::Values,
                    "",
                    "List(Any)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "entries",
                    Op::Entries,
                    "",
                    "List(Tuple(Any, Any))",
                    "sum(1, size(xs), size(result))",
                ),
            ]),
            Kind::Record => ops.extend([
                (
                    "get",
                    Op::Get,
                    ", key: Text",
                    "Any",
                    "sum(1, size(xs), size(key))",
                ),
                (
                    "contains",
                    Op::Contains,
                    ", key: Text",
                    "Bool",
                    "sum(1, size(xs), size(key))",
                ),
                (
                    "keys",
                    Op::Keys,
                    "",
                    "List(Text)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "values",
                    Op::Values,
                    "",
                    "List(Any)",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "entries",
                    Op::Entries,
                    "",
                    "List(Tuple(Text, Any))",
                    "sum(1, size(xs), size(result))",
                ),
                (
                    "with",
                    Op::With,
                    ", key: Text, value: Any",
                    ty,
                    "sum(1, size(xs), size(key), size(result))",
                ),
                (
                    "without",
                    Op::Without,
                    ", key: Text",
                    ty,
                    "sum(1, size(xs), size(key), size(result))",
                ),
            ]),
        }
        if matches!(kind, Kind::List) {
            ops.push((
                "get_or",
                Op::GetOr,
                ", index: Number, default?: Any",
                "Any",
                "sum(1, size(index))",
            ));
        }
        for (name, op, params, result, charge) in ops {
            if matches!(kind, Kind::Tuple) && matches!(op, Op::Check | Op::InsertIndex) {
                continue;
            }
            let definition = define(&format!("{prefix}.{name}"), &|| {
                format!(
                    "function {prefix}.{name}(xs: {ty}{params}) -> {result}\nkernel 1\n\
                 errors \"type_error\", \"index_out_of_range\", \"key_missing\", \"invalid_key\", \"cyclic_value\", \"too_deep\"\ncharge {charge}\nnative\n"
                )
            })?;
            functions.push((definition, Arc::new(Native { kind, op }) as _));
        }
    }
    for (name, function, entry, result, error) in [
        (
            "collection.unordered",
            unordered as fn(&[Value]) -> Result<Value, NativeError>,
            None,
            "Number",
            "unordered",
        ),
        ("collection.zero_step", zero_step, None, "Int", "zero_step"),
        (
            "collection.int_check",
            int_check,
            Some((|args: &[Value]| int_check(args).ok()) as fn(&[Value]) -> Option<Value>),
            "Int",
            "type_error",
        ),
    ] {
        let definition = define(name, &|| {
            format!(
                "function {name}(x: Any) -> {result}\nkernel 1\nerrors \"{error}\"\ncharge sum(1, size(x))\nnative\n"
            )
        })?;
        functions.push((
            definition,
            Arc::new(Aux(
                function,
                entry.map(|entry| PreparedNative {
                    entry,
                    operation: name,
                }),
            )) as _,
        ));
    }
    // Range and NaN equivalence are explicit caller choices. No dialect's
    // coercion, missing-slot convention or index normalization lives here.
    let definition = define("list.index_of_range", &|| {
        String::from(
            "function list.index_of_range(xs: List(Any), value: Any, start: Number, end: Number, reverse: Bool, nan_equal: Bool) -> Int\nkernel 1\nerrors \"type_error\", \"index_out_of_range\"\ncharge sum(8, deep(xs), product(size(xs), deep(value)), size(start), size(end))\nnative\n",
        )
    })?;
    functions.push((definition, Arc::new(IndexOfRange) as _));
    for (signature, function, entry, charge) in [
        (
            "bool.not(x: Bool) -> Bool",
            bool_not as fn(&[Value]) -> Result<Value, NativeError>,
            (|args: &[Value]| bool_not(args).ok()) as fn(&[Value]) -> Option<Value>,
            "1",
        ),
        (
            "error.new(kind: Text, message: Text, data?: Any) -> Error",
            error_new,
            |args| error_new(args).ok(),
            "sum(1, size(kind), size(message), size(result))",
        ),
        (
            "collection.function_check(x: Any) -> Any",
            function_check,
            |args| function_check(args).ok(),
            "1",
        ),
    ] {
        let definition = define(
            signature
                .split_once('(')
                .map_or(signature, |(name, _)| name),
            &|| {
                format!(
                    "function {signature}\nkernel 1\nerrors \"type_error\"\ncharge {charge}\nnative\n"
                )
            },
        )?;
        functions.push((
            definition,
            Arc::new(Aux(
                function,
                Some(PreparedNative {
                    entry,
                    operation: signature,
                }),
            )) as _,
        ));
    }
    let definition = define("collection.holds_none", &|| {
        String::from(
            "function collection.holds_none(value: Any, kinds: Set(Text)) -> Bool\nkernel 1\n\
         errors \"type_error\"\ncharge sum(1, size(kinds), deep(value))\nnative\n",
        )
    })?;
    functions.push((definition, Arc::new(HoldsNone) as _));
    let definition = define("key.identity", &|| {
        String::from(
            "function key.identity(value: Any, absent_key: Any, error_tag: Text) -> Any\nkernel 1\nerrors \"type_error\", \"too_deep\"\ncharge sum(1, deep(result))\nnative\n",
        )
    })?;
    functions.push((definition, Arc::new(IdentityKey) as _));
    Ok(functions)
}
/// `collection.holds_none(value, kinds)`: whether a deep copy of `value`
/// would read no value whose kind `kinds` names, and copy without raising.
struct HoldsNone;
impl NativeFunction for HoldsNone {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let Value::Set(kinds) = arg(&call, 1)? else {
            return Err(raise("type_error", "kinds must be a set of kind names"));
        };
        let mut refused = [false; KIND_NAMES.len()];
        let mut unknown = false;
        let mut fault = None;
        call.heap.visit(*kinds, &mut |element| {
            let named = match element {
                Element::Item(Value::Text(name)) => {
                    let mut named = None;
                    for (index, (_, kind)) in KIND_NAMES.iter().enumerate() {
                        match call
                            .heap
                            .equal(&Value::Text(name.clone()), &Value::text(*kind))
                        {
                            Ok(true) => {
                                named = Some(index);
                                break;
                            }
                            Ok(false) => {}
                            Err(error) => {
                                fault = Some(error);
                                return ControlFlow::Break(());
                            }
                        }
                    }
                    named
                }
                _ => None,
            };
            match named {
                Some(index) => refused[index] = true,
                None => unknown = true,
            }
            ControlFlow::Continue(())
        })?;
        if let Some(error) = fault {
            return Err(error.into());
        }
        if unknown {
            return Err(raise("type_error", "kinds must be a set of kind names"));
        }
        copy::holds_none(arg(&call, 0)?, &refused, call.heap).map(Value::Bool)
    }
}
/// A function of its arguments alone.
struct Aux(
    fn(&[Value]) -> Result<Value, NativeError>,
    Option<PreparedNative>,
);
impl NativeFunction for Aux {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        (self.0)(call.args)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        self.1
    }
}
fn bool_not(args: &[Value]) -> Result<Value, NativeError> {
    match nth(args, 0)? {
        Value::Bool(flag) => Ok(Value::Bool(!flag)),
        _ => Err(raise("type_error", "bool.not requires a bool")),
    }
}
fn error_new(args: &[Value]) -> Result<Value, NativeError> {
    let kind = field(nth(args, 0)?)?.clone();
    let message = field(nth(args, 1)?)?.clone();
    let data = args.get(2).cloned().unwrap_or(Value::Absent);
    let depth = match &data {
        Value::Tuple(t) => t.summary.depth,
        Value::Error(e) => e.depth,
        _ => 0,
    } + 1;
    let has_edges = data.object().is_some()
        || matches!(&data, Value::Task(_) | Value::Ref(_))
        || matches!(&data,Value::Tuple(t) if t.summary.has_edges)
        || matches!(&data,Value::Error(e) if e.has_edges);
    let cost = LogicalBytes(
        kind.summary.utf8_bytes
            + message.summary.utf8_bytes
            + lash_kernel_doc::heap_values::local_cost(&data),
    );
    let task_edges = lash_kernel_doc::heap_values::task_count(&data);
    Ok(Value::Error(Arc::new(ErrorRef {
        family: None,
        task_edges,
        kind,
        message,
        data: Payload::Resident(Arc::new(data)),
        depth,
        cost,
        has_edges,
    })))
}
fn function_check(args: &[Value]) -> Result<Value, NativeError> {
    match nth(args, 0)? {
        value @ (Value::Closure(_) | Value::Function(_)) => Ok(value.clone()),
        _ => Err(raise(
            "type_error",
            "callback must be a closure or function reference",
        )),
    }
}

fn unordered(_: &[Value]) -> Result<Value, NativeError> {
    Err(raise("unordered", "a sort comparator returned NaN"))
}
fn zero_step(_: &[Value]) -> Result<Value, NativeError> {
    Err(raise("zero_step", "a range step must be nonzero"))
}
fn int_check(args: &[Value]) -> Result<Value, NativeError> {
    match nth(args, 0)? {
        Value::Int(n) => Ok(Value::Int(n.clone())),
        _ => Err(raise("type_error", "range arguments must be integers")),
    }
}

fn object(kind: Kind, value: &Value) -> Result<Option<EntityId>, NativeError> {
    match (kind, value) {
        (Kind::List, Value::List(id))
        | (Kind::Map, Value::Map(id))
        | (Kind::Set, Value::Set(id))
        | (Kind::Record, Value::Record(id)) => Ok(Some(*id)),
        (Kind::Tuple, Value::Tuple(_)) => Ok(None),
        _ => Err(raise("type_error", "collection has the wrong kind")),
    }
}
fn arg<'a>(call: &NativeCall<'a>, index: usize) -> Result<&'a Value, NativeError> {
    nth(call.args, index)
}
fn nth(args: &[Value], index: usize) -> Result<&Value, NativeError> {
    args.get(index)
        .ok_or_else(|| raise("arity", "missing collection argument"))
}
fn integer(value: &Value) -> Result<IntRef, NativeError> {
    match value {
        Value::Int(n) => Ok(n.clone()),
        Value::Float(n) if n.get().is_finite() && n.get().fract() == 0.0 => {
            BigInt::from_f64(n.get())
                .map(|n| IntRef::Inline(Integer::new(n)))
                .ok_or_else(|| raise("index_out_of_range", "index must be integral"))
        }
        Value::Float(_) => Err(raise("index_out_of_range", "index must be integral")),
        _ => Err(raise("type_error", "index must be a number")),
    }
}
fn int(n: usize) -> Value {
    Value::Int(IntRef::Inline(Integer::new(BigInt::from(n))))
}
fn negative(n: &IntRef) -> bool {
    match n {
        IntRef::Inline(n) => n.is_negative(),
        IntRef::Paged { summary, .. } => summary.negative,
    }
}
fn machine_index(n: &IntRef) -> Option<usize> {
    match n {
        IntRef::Inline(n) => n.to_usize(),
        _ => None,
    }
}
fn position(value: &IntRef, length: usize, clamp: bool) -> Result<usize, NativeError> {
    let n = match value {
        IntRef::Inline(n) => n.to_i128(),
        _ => None,
    };
    if let Some(mut n) = n {
        if n < 0 {
            n += length as i128;
        }
        if clamp {
            return Ok(n.max(0).min(length as i128) as usize);
        }
        if n >= 0 && n < length as i128 {
            return Ok(n as usize);
        }
    } else if clamp {
        return Ok(if negative(value) { 0 } else { length });
    }
    Err(raise(
        "index_out_of_range",
        "index is outside the collection",
    ))
}
fn key(value: &Value, heap: &dyn NativeHeap) -> Result<(), NativeError> {
    if matches!(value,Value::Tuple(t) if t.summary.depth as usize > heap.value_depth_limit().min(128))
    {
        return Err(raise("too_deep", "key exceeds the configured value depth"));
    }
    heap.normal_key(value)?
        .ok_or_else(|| raise("invalid_key", "expected an immutable key or explicit ref"))?;
    Ok(())
}
fn field(value: &Value) -> Result<&TextRef, NativeError> {
    match value {
        Value::Text(text) => Ok(text),
        _ => Err(raise("type_error", "field name must be text")),
    }
}

pub(super) fn snapshot(
    value: &Value,
    heap: &dyn lash_kernel_doc::NativeHeap,
) -> Result<Object, NativeError> {
    let mut items = Vec::new();
    let mut entries = Vec::new();
    let mut fields = Vec::new();
    let Some(id) = value.object() else {
        return Err(raise("type_error", "expected a collection"));
    };
    heap.visit(id, &mut |element| {
        match element {
            Element::Item(item) => items.push(item.clone()),
            Element::Entry { key, value } => entries.push((key.clone(), value.clone())),
            Element::Field { name, value } => fields.push((name.clone(), value.clone())),
        }
        ControlFlow::Continue(())
    })?;
    match value {
        Value::List(_) => Ok(Object::List(items)),
        Value::Map(_) => Ok(Object::Map(entries)),
        Value::Set(_) => Ok(Object::Set(items)),
        Value::Record(_) => Ok(Object::Record(fields)),
        _ => Err(raise("type_error", "expected a collection")),
    }
}
fn visit_sequence(
    value: &Value,
    heap: &dyn NativeHeap,
    visitor: &mut dyn FnMut(&Value) -> ControlFlow<()>,
) -> Result<(), HeapFault> {
    match value {
        Value::Tuple(t) => heap.tuple_visit(t, visitor),
        Value::List(id) => heap.visit(*id, &mut |e| match e {
            Element::Item(v) => visitor(v),
            _ => ControlFlow::Break(()),
        }),
        _ => Err(HeapFault::Malformed("sequence has another kind".into())),
    }
}
fn append_range(
    value: &Value,
    heap: &dyn NativeHeap,
    start: usize,
    end: usize,
    items: &mut Vec<Value>,
) -> Result<(), HeapFault> {
    for index in start..end {
        let value = match value {
            Value::Tuple(t) => heap.tuple_get(t, index as u64)?,
            Value::List(id) => heap.list_get(*id, index as u64)?,
            _ => None,
        };
        items.push(
            value.ok_or_else(|| {
                HeapFault::Malformed("an admitted sequence member is absent".into())
            })?,
        );
    }
    Ok(())
}
fn result_items(
    heap: &mut dyn lash_kernel_doc::NativeHeap,
    count: usize,
    values_per_item: u64,
) -> Result<Vec<Value>, NativeError> {
    heap.reserve((count as u64).saturating_mul(values_per_item), 0)?;
    let mut items = Vec::new();
    items
        .try_reserve_exact(count)
        .map_err(|_| NativeError::Memory)?;
    Ok(items)
}
enum Projection {
    Value(Value),
    Pair(Value, Value),
}
fn view_item(op: Op, kind: Kind, index: usize, element: Element<'_>) -> Projection {
    let key = || match element {
        Element::Item(v) if matches!(kind, Kind::Set) => v.clone(),
        Element::Item(_) => int(index),
        Element::Entry { key, .. } => key.clone(),
        Element::Field { name, .. } => Value::Text(name.clone()),
    };
    let value = || match element {
        Element::Item(v) | Element::Entry { value: v, .. } | Element::Field { value: v, .. } => {
            v.clone()
        }
    };
    match op {
        Op::Keys => Projection::Value(key()),
        Op::Values => Projection::Value(value()),
        Op::Entries => Projection::Pair(key(), value()),
        _ => unreachable!(),
    }
}
fn allocate(kind: Kind, object: Object, call: &mut NativeCall<'_>) -> Result<Value, NativeError> {
    let id = call.heap.allocate(object)?;
    Ok(match kind {
        Kind::List => Value::List(id),
        Kind::Map => Value::Map(id),
        Kind::Set => Value::Set(id),
        Kind::Record => Value::Record(id),
        Kind::Tuple => unreachable!(),
    })
}
impl NativeFunction for Native {
    fn call(&self, mut call: NativeCall<'_>) -> Result<Value, NativeError> {
        if let Some(result) = self.read(call.args, &*call.heap) {
            return result;
        }
        let xs = arg(&call, 0)?.clone();
        let id = object(self.kind, &xs)?;
        let length = match &xs {
            Value::Tuple(items) => items.summary.len as usize,
            _ => match id {
                Some(id) => call.heap.len(id)? as usize,
                None => 0,
            },
        };
        match self.op {
            Op::Len
            | Op::Check
            | Op::InsertIndex
            | Op::Get
            | Op::GetOr
            | Op::At
            | Op::Contains
            | Op::IndexOf => unreachable!("an operation that only reads was read above"),
            Op::Copy => match xs {
                Value::Tuple(_) => Ok(xs),
                _ => {
                    let obj = snapshot(&xs, call.heap)?;
                    allocate(self.kind, obj, &mut call)
                }
            },
            Op::DeepCopy => copy::deep(&xs, call.heap),
            Op::Slice | Op::Concat => {
                let items = if matches!(self.op, Op::Concat) {
                    let other = arg(&call, 1)?;
                    let other_id = object(self.kind, other)?;
                    let other_length = match other {
                        Value::Tuple(items) => items.summary.len as usize,
                        _ => match other_id {
                            Some(id) => call.heap.len(id)? as usize,
                            None => 0,
                        },
                    };
                    let count = length
                        .checked_add(other_length)
                        .ok_or(NativeError::Memory)?;
                    let mut items = result_items(call.heap, count, 1)?;
                    append_range(&xs, call.heap, 0, length, &mut items)?;
                    append_range(other, call.heap, 0, other_length, &mut items)?;
                    items
                } else {
                    let start = position(&integer(arg(&call, 1)?)?, length, true)?;
                    let end = match call.args.get(2) {
                        Some(Value::Absent) | None => length,
                        Some(end) => position(&integer(end)?, length, true)?,
                    }
                    .max(start);
                    let mut items = result_items(call.heap, end - start, 1)?;
                    append_range(&xs, call.heap, start, end, &mut items)?;
                    items
                };
                match self.kind {
                    Kind::Tuple => Ok(Value::Tuple(call.heap.tuple(items)?)),
                    _ => allocate(Kind::List, Object::List(items), &mut call),
                }
            }
            Op::Keys | Op::Values | Op::Entries => {
                let mut items = result_items(
                    call.heap,
                    length,
                    if matches!(self.op, Op::Entries) { 3 } else { 1 },
                )?;
                if matches!(self.op, Op::Keys) && matches!(self.kind, Kind::List | Kind::Tuple) {
                    items.extend((0..length).map(int));
                } else {
                    let mut projected = Vec::new();
                    let mut visit = |e: Element<'_>| {
                        projected.push(view_item(self.op, self.kind, projected.len(), e));
                        ControlFlow::Continue(())
                    };
                    match &xs {
                        Value::Tuple(t) => {
                            call.heap.tuple_visit(t, &mut |v| visit(Element::Item(v)))?
                        }
                        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                            call.heap.visit(*id, &mut visit)?
                        }
                        _ => unreachable!(),
                    }
                    for projection in projected {
                        items.push(match projection {
                            Projection::Value(v) => v,
                            Projection::Pair(k, v) => Value::Tuple(call.heap.tuple(vec![k, v])?),
                        });
                    }
                }
                allocate(Kind::List, Object::List(items), &mut call)
            }
            Op::With | Op::Without => {
                let name = field(arg(&call, 1)?)?.clone();
                let Object::Record(mut fields) = snapshot(&xs, call.heap)? else {
                    unreachable!()
                };
                let mut at = None;
                for (index, (key, _)) in fields.iter().enumerate() {
                    if call
                        .heap
                        .equal(&Value::Text(key.clone()), &Value::Text(name.clone()))?
                    {
                        at = Some(index);
                        break;
                    }
                }
                if matches!(self.op, Op::Without) {
                    if let Some(at) = at {
                        fields.remove(at);
                    }
                } else {
                    let value = arg(&call, 2)?.clone();
                    match at {
                        Some(at) => fields[at].1 = value,
                        None => fields.push((name, value)),
                    }
                }
                allocate(Kind::Record, Object::Record(fields), &mut call)
            }
        }
    }

    fn prepare(&self) -> Option<PreparedNative> {
        if !matches!(self.op, Op::Check) {
            return None;
        }
        macro_rules! check {
            ($kind:ident) => {
                PreparedNative {
                    operation: concat!(stringify!($kind), ".check"),
                    entry: |args| {
                        let value = args.first()?;
                        object(Kind::$kind, value).ok()?;
                        Some(value.clone())
                    },
                }
            };
        }
        Some(match self.kind {
            Kind::List => check!(List),
            Kind::Tuple => check!(Tuple),
            Kind::Map => check!(Map),
            Kind::Set => check!(Set),
            Kind::Record => check!(Record),
        })
    }
}

impl Native {
    /// The call of an operation that only reads the heap: it allocates and
    /// reserves nothing. `None` for an operation that does.
    fn read(&self, args: &[Value], heap: &dyn NativeHeap) -> Option<Result<Value, NativeError>> {
        matches!(
            self.op,
            Op::Len
                | Op::Check
                | Op::InsertIndex
                | Op::Get
                | Op::GetOr
                | Op::At
                | Op::Contains
                | Op::IndexOf
        )
        .then(|| self.reading(args, heap))
    }

    fn reading(&self, args: &[Value], heap: &dyn NativeHeap) -> Result<Value, NativeError> {
        let xs = nth(args, 0)?;
        let id = object(self.kind, xs)?;
        let length = match xs {
            Value::Tuple(items) => items.summary.len as usize,
            _ => match id {
                Some(id) => heap.len(id)? as usize,
                None => 0,
            },
        };
        match self.op {
            Op::Len => Ok(int(length)),
            Op::Check => Ok(xs.clone()),
            Op::InsertIndex => {
                let n = integer(nth(args, 1)?)?;
                if machine_index(&n).is_none_or(|n| n > length) {
                    return Err(raise(
                        "index_out_of_range",
                        "insert position is outside the collection",
                    ));
                }
                Ok(Value::Int(n))
            }
            Op::Get | Op::GetOr | Op::At => match xs {
                Value::List(id) => {
                    let index = integer(nth(args, 1)?)?;
                    if matches!(self.op, Op::Get | Op::GetOr) && negative(&index) {
                        return Err(raise(
                            "index_out_of_range",
                            "get requires a nonnegative index",
                        ));
                    }
                    if matches!(self.op, Op::GetOr) {
                        return Ok(match machine_index(&index) {
                            Some(i) => heap.list_get(*id, i as u64)?,
                            None => None,
                        }
                        .unwrap_or_else(|| args.get(2).cloned().unwrap_or(Value::Absent)));
                    }
                    let i = position(&index, length, false)?;
                    heap.list_get(*id, i as u64)?
                        .ok_or_else(|| raise("index_out_of_range", "missing list element"))
                }
                Value::Tuple(items) => {
                    let index = integer(nth(args, 1)?)?;
                    if matches!(self.op, Op::Get | Op::GetOr) && negative(&index) {
                        return Err(raise(
                            "index_out_of_range",
                            "get requires a nonnegative index",
                        ));
                    }
                    heap.tuple_get(items, position(&index, length, false)? as u64)?
                        .ok_or_else(|| raise("index_out_of_range", "missing tuple element"))
                }
                Value::Map(id) => {
                    let k = nth(args, 1)?;
                    key(k, heap)?;
                    heap.map_get(*id, k)?
                        .ok_or_else(|| raise("key_missing", "map has no such key"))
                }
                Value::Record(id) => Ok(heap
                    .record_get(*id, FieldName::Text(field(nth(args, 1)?)?))?
                    .unwrap_or(Value::Absent)),
                _ => Err(raise("type_error", "collection has no get")),
            },
            Op::Contains | Op::IndexOf => {
                let value = nth(args, 1)?;
                let found = match xs {
                    Value::Map(id) => {
                        key(value, heap)?;
                        Some(heap.map_get(*id, value)?.is_some())
                    }
                    Value::Set(id) => {
                        key(value, heap)?;
                        Some(heap.set_contains(*id, value)?)
                    }
                    Value::Record(id) => Some(
                        heap.record_get(*id, FieldName::Text(field(value)?))?
                            .is_some(),
                    ),
                    _ => None,
                };
                if let Some(found) = found {
                    return Ok(Value::Bool(found));
                }
                let mut found = None;
                let mut index = 0;
                let mut fault = None;
                visit_sequence(xs, heap, &mut |item| match heap.equal(item, value) {
                    Ok(true) => {
                        found = Some(index);
                        ControlFlow::Break(())
                    }
                    Ok(false) => {
                        index += 1;
                        ControlFlow::Continue(())
                    }
                    Err(error) => {
                        fault = Some(error);
                        ControlFlow::Break(())
                    }
                })?;
                if let Some(error) = fault {
                    return Err(error.into());
                }
                Ok(match self.op {
                    Op::IndexOf => found.map_or(Value::Int(IntRef::Inline(Integer::from(-1))), int),
                    _ => Value::Bool(found.is_some()),
                })
            }
            _ => unreachable!("only an operation that reads is read"),
        }
    }
}

/// Searches an absolute half-open range using kernel equality, optionally
/// equating floating NaNs. Reverse selects the last match in the range.
struct IndexOfRange;
impl NativeFunction for IndexOfRange {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        index_of_range(call)
    }
}
fn index_of_range(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let Value::List(id) = arg(&call, 0)? else {
        return Err(raise("type_error", "expected list"));
    };
    let needle = arg(&call, 1)?;
    let length = call.heap.len(*id)? as usize;
    let (Some(start), Some(end)) = (
        machine_index(&integer(arg(&call, 2)?)?),
        machine_index(&integer(arg(&call, 3)?)?),
    ) else {
        return Err(raise(
            "index_out_of_range",
            "range requires nonnegative indices",
        ));
    };
    if start > end || end > length {
        return Err(raise("index_out_of_range", "range is outside the list"));
    }
    let (Value::Bool(reverse), Value::Bool(nan_equal)) = (arg(&call, 4)?, arg(&call, 5)?) else {
        return Err(raise("type_error", "search options require booleans"));
    };
    let mut found = None;
    for at in 0..end - start {
        let index = if *reverse { end - at - 1 } else { start + at };
        let value = call
            .heap
            .list_get(*id, index as u64)?
            .ok_or_else(|| raise("index_out_of_range", "missing list element"))?;
        if call.heap.equal(&value, needle)?
            || (*nan_equal
                && matches!((&value,needle),(Value::Float(a),Value::Float(b)) if a.get().is_nan()&&b.get().is_nan()))
        {
            found = Some(index);
            break;
        }
    }
    // Even the scalar result uses the run's remaining room before its
    // integer digits are allocated. Zero has no magnitude bytes; -1 has one.
    let bytes = found.map_or(1, |index| {
        u64::from(usize::BITS - index.leading_zeros()).div_ceil(8)
    });
    call.heap.reserve(0, bytes)?;
    Ok(found.map_or_else(|| Value::Int(IntRef::Inline(Integer::from(-1))), int))
}

/// K-COL-017: project immutable tuple structure, never object contents.
/// Encodings for absent and errors are supplied by the caller; no source
/// dialect's tokens or kind names are recognized here.
struct IdentityKey;
impl NativeFunction for IdentityKey {
    fn call(&self, mut call: NativeCall<'_>) -> Result<Value, NativeError> {
        let value = arg(&call, 0)?.clone();
        let absent = arg(&call, 1)?.clone();
        let tag = arg(&call, 2)?.clone();
        field(&tag)?;
        project_identity(&value, &absent, &tag, 0, &mut call)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            operation: "collection.identity_key(Any, Any, Text)",
            entry: |args| {
                field(args.get(2)?).ok()?;
                scalar_identity(args.first()?, args.get(1)?)
            },
        })
    }
}

fn scalar_identity(value: &Value, absent: &Value) -> Option<Value> {
    Some(match value {
        Value::Tuple(_) | Value::Error(_) => return None,
        Value::Absent => absent.clone(),
        Value::Float(n) if n.get() == 0.0 => Value::Float(Float::new(0.0)),
        Value::Task(task) => Value::Ref(Identity::Task(*task)),
        other => other
            .object()
            .map_or_else(|| other.clone(), |id| Value::Ref(Identity::Object(id))),
    })
}

fn project_identity(
    value: &Value,
    absent: &Value,
    tag: &Value,
    depth: usize,
    call: &mut NativeCall<'_>,
) -> Result<Value, NativeError> {
    if depth > call.heap.value_depth_limit().min(128) {
        return Err(raise(
            "too_deep",
            "identity key exceeds the configured value depth",
        ));
    }
    call.counter.spend(1)?;
    if let Some(value) = scalar_identity(value, absent) {
        return Ok(value);
    }
    match value {
        Value::Tuple(tuple) => {
            call.heap.reserve(tuple.summary.len, 0)?;
            let mut projected = Vec::new();
            for index in 0..tuple.summary.len {
                let item = call
                    .heap
                    .tuple_get(tuple, index)?
                    .ok_or_else(|| raise("type_error", "missing tuple member"))?;
                projected.push(project_identity(&item, absent, tag, depth + 1, call)?);
            }
            Ok(Value::Tuple(call.heap.tuple(projected)?))
        }
        Value::Error(error) => {
            let held = call.heap.error_data(error)?;
            let data = match &held {
                Value::Task(task) => Value::Ref(Identity::Task(*task)),
                other => {
                    Value::Ref(Identity::Object(other.object().ok_or_else(|| {
                        raise("type_error", "ref requires an object or task")
                    })?))
                }
            };
            call.heap.reserve(4, 0)?;
            Ok(Value::Tuple(call.heap.tuple(vec![
                tag.clone(),
                data,
                Value::Text(error.kind.clone()),
                Value::Text(error.message.clone()),
            ])?))
        }
        _ => unreachable!("scalar identity covered other values"),
    }
}
