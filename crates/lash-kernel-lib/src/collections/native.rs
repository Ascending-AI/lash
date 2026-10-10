use std::ops::ControlFlow;
use std::sync::Arc;

use lash_kernel_doc::{
    Element, FunctionDefinition, Integer, NativeCall, NativeError, NativeFunction, NativeHeap,
    Object, ObjectId, Value, parse_definition,
};
use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive};

use super::{CollectionError, copy, raise};

type NativeDefinition = (FunctionDefinition, Arc<dyn NativeFunction>);

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

pub(super) fn functions() -> Result<Vec<NativeDefinition>, CollectionError> {
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
        for (name, op, params, result, charge) in ops {
            if matches!(kind, Kind::Tuple) && matches!(op, Op::Check | Op::InsertIndex) {
                continue;
            }
            let definition = parse_definition(&format!(
                "function {prefix}.{name}(xs: {ty}{params}) -> {result}\nkernel 1\n\
                 errors \"type_error\", \"index_out_of_range\", \"key_missing\", \"invalid_key\", \"cyclic_value\", \"too_deep\"\ncharge {charge}\nnative\n"
            )).map_err(CollectionError::Parse)?;
            functions.push((definition, Arc::new(Native { kind, op }) as _));
        }
    }
    for (name, function, result, error) in [
        (
            "collection.unordered",
            unordered as fn(&[Value]) -> Result<Value, NativeError>,
            "Number",
            "unordered",
        ),
        ("collection.zero_step", zero_step, "Int", "zero_step"),
        ("collection.int_check", int_check, "Int", "type_error"),
    ] {
        let definition = parse_definition(&format!(
            "function {name}(x: Any) -> {result}\nkernel 1\nerrors \"{error}\"\ncharge sum(1, size(x))\nnative\n"
        )).map_err(CollectionError::Parse)?;
        functions.push((definition, Arc::new(Aux(function)) as _));
    }
    for (signature, function, charge) in [
        (
            "bool.not(x: Bool) -> Bool",
            bool_not as fn(&[Value]) -> Result<Value, NativeError>,
            "1",
        ),
        (
            "error.new(kind: Text, message: Text, data?: Any) -> Error",
            error_new,
            "sum(1, size(kind), size(message), size(result))",
        ),
        (
            "collection.function_check(x: Any) -> Any",
            function_check,
            "1",
        ),
    ] {
        let definition = parse_definition(&format!(
            "function {signature}\nkernel 1\nerrors \"type_error\"\ncharge {charge}\nnative\n"
        ))
        .map_err(CollectionError::Parse)?;
        functions.push((definition, Arc::new(Aux(function)) as _));
    }
    Ok(functions)
}
/// A function of its arguments alone.
struct Aux(fn(&[Value]) -> Result<Value, NativeError>);
impl NativeFunction for Aux {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        (self.0)(call.args)
    }

    fn fast(&self, args: &[Value], _heap: &dyn NativeHeap) -> Option<Value> {
        (self.0)(args).ok()
    }
}
fn bool_not(args: &[Value]) -> Result<Value, NativeError> {
    match nth(args, 0)? {
        Value::Bool(flag) => Ok(Value::Bool(!flag)),
        _ => Err(raise("type_error", "bool.not requires a bool")),
    }
}
fn error_new(args: &[Value]) -> Result<Value, NativeError> {
    let kind = field(nth(args, 0)?)?.to_owned();
    let message = field(nth(args, 1)?)?.to_owned();
    Ok(Value::Error(Arc::new(lash_kernel_doc::ErrorValue {
        kind,
        message,
        data: args.get(2).cloned().unwrap_or(Value::Absent),
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

fn object(kind: Kind, value: &Value) -> Result<Option<ObjectId>, NativeError> {
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
fn integer(value: &Value) -> Result<Integer, NativeError> {
    match value {
        Value::Int(n) => Ok(n.clone()),
        Value::Float(n) if n.get().is_finite() && n.get().fract() == 0.0 => {
            if let Some(n) = n.get().to_i64() {
                Ok(Integer::from(n))
            } else {
                BigInt::from_f64(n.get())
                    .map(Integer::new)
                    .ok_or_else(|| raise("index_out_of_range", "index must be integral"))
            }
        }
        Value::Float(_) => Err(raise("index_out_of_range", "index must be integral")),
        _ => Err(raise("type_error", "index must be a number")),
    }
}
fn int(n: usize) -> Value {
    Value::Int(match i64::try_from(n) {
        Ok(n) => Integer::from(n),
        Err(_) => Integer::new(BigInt::from(n)),
    })
}
fn position(value: &Integer, length: usize, clamp: bool) -> Result<usize, NativeError> {
    if let Some(mut n) = value.to_i128() {
        let length = length as i128;
        if n < 0 {
            n += length;
        }
        if clamp {
            n = n.max(0).min(length);
        } else if n < 0 || n >= length {
            return Err(raise(
                "index_out_of_range",
                "index is outside the collection",
            ));
        }
        return usize::try_from(n)
            .map_err(|_| raise("index_out_of_range", "index is outside the collection"));
    }
    let mut n = value.as_bigint().into_owned();
    let length = BigInt::from(length);
    if n < BigInt::from(0) {
        n += &length;
    }
    if clamp {
        n = n.max(BigInt::from(0)).min(length);
    } else if n < BigInt::from(0) || n >= length {
        return Err(raise(
            "index_out_of_range",
            "index is outside the collection",
        ));
    }
    n.to_usize()
        .ok_or_else(|| raise("index_out_of_range", "index is outside the collection"))
}
fn key(value: &Value, depth: usize) -> Result<(), NativeError> {
    if depth > 128 {
        return Err(raise("too_deep", "key is too deep"));
    }
    match value {
        Value::Null
        | Value::Bool(_)
        | Value::Int(_)
        | Value::Float(_)
        | Value::Text(_)
        | Value::Bytes(_)
        | Value::Timestamp(_)
        | Value::Function(_)
        | Value::Ref(_) => Ok(()),
        Value::Tuple(items) => {
            for item in items.iter() {
                key(item, depth + 1)?;
            }
            Ok(())
        }
        _ => Err(raise(
            "invalid_key",
            "expected an immutable key or explicit ref",
        )),
    }
}
fn field(value: &Value) -> Result<&str, NativeError> {
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
            Element::Field { name, value } => fields.push((name.to_owned(), value.clone())),
        }
        ControlFlow::Continue(())
    });
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
    heap: &dyn lash_kernel_doc::NativeHeap,
    visitor: &mut dyn FnMut(&Value) -> ControlFlow<()>,
) {
    match value {
        Value::Tuple(items) => {
            let _ = items.iter().try_for_each(visitor);
        }
        Value::List(id) => heap.visit(*id, &mut |element| match element {
            Element::Item(item) => visitor(item),
            _ => unreachable!(),
        }),
        _ => unreachable!(),
    }
}
fn append_range(
    value: &Value,
    heap: &dyn lash_kernel_doc::NativeHeap,
    start: usize,
    end: usize,
    items: &mut Vec<Value>,
) {
    if start >= end {
        return;
    }
    if let Value::Tuple(source) = value {
        items.extend_from_slice(&source[start..end]);
        return;
    }
    let mut index = 0;
    visit_sequence(value, heap, &mut |item| {
        if index >= start {
            items.push(item.clone());
        }
        index += 1;
        if index == end {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    });
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
fn view_item(op: Op, kind: Kind, index: usize, element: Element<'_>) -> Value {
    let key = || match element {
        Element::Item(item) if matches!(kind, Kind::Set) => item.clone(),
        Element::Item(_) => int(index),
        Element::Entry { key, .. } => key.clone(),
        Element::Field { name, .. } => Value::text(name),
    };
    let value = || match element {
        Element::Item(item)
        | Element::Entry { value: item, .. }
        | Element::Field { value: item, .. } => item.clone(),
    };
    match op {
        Op::Keys => key(),
        Op::Values => value(),
        Op::Entries => Value::Tuple(vec![key(), value()].into()),
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
            Value::Tuple(items) => items.len(),
            _ => id.map_or(0, |id| call.heap.len(id)),
        };
        match self.op {
            Op::Len
            | Op::Check
            | Op::InsertIndex
            | Op::Get
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
                        Value::Tuple(items) => items.len(),
                        _ => other_id.map_or(0, |id| call.heap.len(id)),
                    };
                    let count = length
                        .checked_add(other_length)
                        .ok_or(NativeError::Memory)?;
                    let mut items = result_items(call.heap, count, 1)?;
                    append_range(&xs, call.heap, 0, length, &mut items);
                    append_range(other, call.heap, 0, other_length, &mut items);
                    items
                } else {
                    let start = position(&integer(arg(&call, 1)?)?, length, true)?;
                    let end = match call.args.get(2) {
                        Some(Value::Absent) | None => length,
                        Some(end) => position(&integer(end)?, length, true)?,
                    }
                    .max(start);
                    let mut items = result_items(call.heap, end - start, 1)?;
                    append_range(&xs, call.heap, start, end, &mut items);
                    items
                };
                match self.kind {
                    Kind::Tuple => Ok(Value::Tuple(items.into())),
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
                    let mut visit = |element: Element<'_>| {
                        items.push(view_item(self.op, self.kind, items.len(), element));
                        ControlFlow::Continue(())
                    };
                    match &xs {
                        Value::Tuple(source) => {
                            let _ = source
                                .iter()
                                .try_for_each(|item| visit(Element::Item(item)));
                        }
                        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
                            call.heap.visit(*id, &mut visit);
                        }
                        _ => unreachable!(),
                    }
                }
                allocate(Kind::List, Object::List(items), &mut call)
            }
            Op::With | Op::Without => {
                let name = field(arg(&call, 1)?)?.to_owned();
                let Object::Record(mut fields) = snapshot(&xs, call.heap)? else {
                    unreachable!()
                };
                let at = fields.iter().position(|(k, _)| k == &name);
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

    fn fast(&self, args: &[Value], heap: &dyn NativeHeap) -> Option<Value> {
        self.read(args, heap)?.ok()
    }
}

impl Native {
    /// The call of an operation that only reads the heap: it allocates and
    /// reserves nothing. `None` for an operation that does.
    fn read(&self, args: &[Value], heap: &dyn NativeHeap) -> Option<Result<Value, NativeError>> {
        matches!(
            self.op,
            Op::Len | Op::Check | Op::InsertIndex | Op::Get | Op::At | Op::Contains | Op::IndexOf
        )
        .then(|| self.reading(args, heap))
    }

    fn reading(&self, args: &[Value], heap: &dyn NativeHeap) -> Result<Value, NativeError> {
        let xs = nth(args, 0)?;
        let id = object(self.kind, xs)?;
        let length = match xs {
            Value::Tuple(items) => items.len(),
            _ => id.map_or(0, |id| heap.len(id)),
        };
        match self.op {
            Op::Len => Ok(int(length)),
            Op::Check => Ok(xs.clone()),
            Op::InsertIndex => {
                let n = integer(nth(args, 1)?)?;
                if n.to_usize().is_none_or(|n| n > length) {
                    return Err(raise(
                        "index_out_of_range",
                        "insert position is outside the collection",
                    ));
                }
                Ok(Value::Int(n))
            }
            Op::Get | Op::At => match xs {
                Value::List(id) => {
                    let index = integer(nth(args, 1)?)?;
                    if matches!(self.op, Op::Get) && index.is_negative() {
                        return Err(raise(
                            "index_out_of_range",
                            "get requires a nonnegative index",
                        ));
                    }
                    let i = position(&index, length, false)?;
                    heap.list_get(*id, i)
                        .ok_or_else(|| raise("index_out_of_range", "missing list element"))
                }
                Value::Tuple(items) => {
                    let index = integer(nth(args, 1)?)?;
                    if matches!(self.op, Op::Get) && index.is_negative() {
                        return Err(raise(
                            "index_out_of_range",
                            "get requires a nonnegative index",
                        ));
                    }
                    Ok(items[position(&index, length, false)?].clone())
                }
                Value::Map(id) => {
                    let k = nth(args, 1)?;
                    key(k, 0)?;
                    heap.map_get(*id, k)
                        .ok_or_else(|| raise("key_missing", "map has no such key"))
                }
                Value::Record(id) => Ok(heap
                    .record_get(*id, field(nth(args, 1)?)?)
                    .unwrap_or(Value::Absent)),
                _ => Err(raise("type_error", "collection has no get")),
            },
            Op::Contains | Op::IndexOf => {
                let value = nth(args, 1)?;
                let found = match xs {
                    Value::Map(id) => {
                        key(value, 0)?;
                        Some(heap.map_get(*id, value).is_some())
                    }
                    Value::Set(id) => {
                        key(value, 0)?;
                        Some(heap.set_contains(*id, value))
                    }
                    Value::Record(id) => Some(heap.record_get(*id, field(value)?).is_some()),
                    _ => None,
                };
                if let Some(found) = found {
                    return Ok(Value::Bool(found));
                }
                let mut found = None;
                let mut index = 0;
                visit_sequence(xs, heap, &mut |item| {
                    if crate::equal(item, value, heap) {
                        found = Some(index);
                        ControlFlow::Break(())
                    } else {
                        index += 1;
                        ControlFlow::Continue(())
                    }
                });
                Ok(match self.op {
                    Op::IndexOf => found.map_or(Value::Int(Integer::from(-1)), int),
                    _ => Value::Bool(found.is_some()),
                })
            }
            _ => unreachable!("only an operation that reads is read"),
        }
    }
}
