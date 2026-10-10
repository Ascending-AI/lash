use std::ops::ControlFlow;
use std::sync::Arc;

use lash_kernel_doc::{
    Element, FunctionDefinition, Integer, NativeCall, NativeError, NativeFunction, Object,
    ObjectId, Value, parse_definition,
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
            unordered as fn(NativeCall<'_>) -> Result<Value, NativeError>,
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
            bool_not as fn(NativeCall<'_>) -> Result<Value, NativeError>,
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
struct Aux(fn(NativeCall<'_>) -> Result<Value, NativeError>);
impl NativeFunction for Aux {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        (self.0)(call)
    }
}
fn bool_not(call: NativeCall<'_>) -> Result<Value, NativeError> {
    match arg(&call, 0)? {
        Value::Bool(flag) => Ok(Value::Bool(!flag)),
        _ => Err(raise("type_error", "bool.not requires a bool")),
    }
}
fn error_new(call: NativeCall<'_>) -> Result<Value, NativeError> {
    let kind = field(arg(&call, 0)?)?.to_owned();
    let message = field(arg(&call, 1)?)?.to_owned();
    Ok(Value::Error(Arc::new(lash_kernel_doc::ErrorValue {
        kind,
        message,
        data: call.args.get(2).cloned().unwrap_or(Value::Absent),
    })))
}
fn function_check(call: NativeCall<'_>) -> Result<Value, NativeError> {
    match arg(&call, 0)? {
        value @ (Value::Closure(_) | Value::Function(_)) => Ok(value.clone()),
        _ => Err(raise(
            "type_error",
            "callback must be a closure or function reference",
        )),
    }
}

fn unordered(_: NativeCall<'_>) -> Result<Value, NativeError> {
    Err(raise("unordered", "a sort comparator returned NaN"))
}
fn zero_step(_: NativeCall<'_>) -> Result<Value, NativeError> {
    Err(raise("zero_step", "a range step must be nonzero"))
}
fn int_check(call: NativeCall<'_>) -> Result<Value, NativeError> {
    match arg(&call, 0)? {
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
fn arg<'a>(call: &'a NativeCall<'_>, index: usize) -> Result<&'a Value, NativeError> {
    call.args
        .get(index)
        .ok_or_else(|| raise("arity", "missing collection argument"))
}
fn integer(value: &Value) -> Result<BigInt, NativeError> {
    match value {
        Value::Int(n) => Ok(n.as_bigint().into_owned()),
        Value::Float(n) if n.get().is_finite() && n.get().fract() == 0.0 => {
            BigInt::from_f64(n.get())
                .ok_or_else(|| raise("index_out_of_range", "index must be integral"))
        }
        Value::Float(_) => Err(raise("index_out_of_range", "index must be integral")),
        _ => Err(raise("type_error", "index must be a number")),
    }
}
fn int(n: usize) -> Value {
    Value::Int(Integer::new(BigInt::from(n)))
}
fn position(value: &Value, length: usize, clamp: bool) -> Result<usize, NativeError> {
    let mut n = integer(value)?;
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
fn members(
    value: &Value,
    heap: &dyn lash_kernel_doc::NativeHeap,
) -> Result<Vec<Value>, NativeError> {
    match value {
        Value::Tuple(items) => Ok(items.to_vec()),
        Value::List(_) => match snapshot(value, heap)? {
            Object::List(items) => Ok(items),
            _ => unreachable!(),
        },
        _ => Err(raise("type_error", "expected a list or tuple")),
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
        let xs = arg(&call, 0)?.clone();
        let id = object(self.kind, &xs)?;
        let length = match &xs {
            Value::Tuple(items) => items.len(),
            _ => id.map_or(0, |id| call.heap.len(id)),
        };
        match self.op {
            Op::Len => Ok(int(length)),
            Op::Check => Ok(xs),
            Op::Copy => match xs {
                Value::Tuple(_) => Ok(xs),
                _ => {
                    let obj = snapshot(&xs, call.heap)?;
                    allocate(self.kind, obj, &mut call)
                }
            },
            Op::DeepCopy => copy::deep(&xs, call.heap),
            Op::InsertIndex => {
                let n = integer(arg(&call, 1)?)?;
                if n < BigInt::from(0) || n > BigInt::from(length) {
                    return Err(raise(
                        "index_out_of_range",
                        "insert position is outside the collection",
                    ));
                }
                Ok(Value::Int(Integer::new(n)))
            }
            Op::Get | Op::At => match xs {
                Value::List(id) => {
                    let index = arg(&call, 1)?;
                    if matches!(self.op, Op::Get) && integer(index)? < BigInt::from(0) {
                        return Err(raise(
                            "index_out_of_range",
                            "get requires a nonnegative index",
                        ));
                    }
                    let i = position(index, length, false)?;
                    call.heap
                        .list_get(id, i)
                        .ok_or_else(|| raise("index_out_of_range", "missing list element"))
                }
                Value::Tuple(items) => {
                    let index = arg(&call, 1)?;
                    if matches!(self.op, Op::Get) && integer(index)? < BigInt::from(0) {
                        return Err(raise(
                            "index_out_of_range",
                            "get requires a nonnegative index",
                        ));
                    }
                    Ok(items[position(index, length, false)?].clone())
                }
                Value::Map(id) => {
                    let k = arg(&call, 1)?;
                    key(k, 0)?;
                    call.heap
                        .map_get(id, k)
                        .ok_or_else(|| raise("key_missing", "map has no such key"))
                }
                Value::Record(id) => Ok(call
                    .heap
                    .record_get(id, field(arg(&call, 1)?)?)
                    .unwrap_or(Value::Absent)),
                _ => Err(raise("type_error", "collection has no get")),
            },
            Op::Contains | Op::IndexOf => {
                let value = arg(&call, 1)?.clone();
                let found = match xs {
                    Value::Map(id) => {
                        key(&value, 0)?;
                        Some(call.heap.map_get(id, &value).is_some())
                    }
                    Value::Set(id) => {
                        key(&value, 0)?;
                        Some(call.heap.set_contains(id, &value))
                    }
                    Value::Record(id) => Some(call.heap.record_get(id, field(&value)?).is_some()),
                    _ => None,
                };
                if let Some(found) = found {
                    return Ok(Value::Bool(found));
                }
                for (i, item) in members(&xs, call.heap)?.into_iter().enumerate() {
                    if crate::equal(&item, &value, call.heap) {
                        return Ok(match self.op {
                            Op::IndexOf => int(i),
                            _ => Value::Bool(true),
                        });
                    }
                }
                Ok(match self.op {
                    Op::IndexOf => Value::Int(Integer::from(-1)),
                    _ => Value::Bool(false),
                })
            }
            Op::Slice | Op::Concat => {
                let mut items = members(&xs, call.heap)?;
                if matches!(self.op, Op::Concat) {
                    let other = arg(&call, 1)?;
                    object(self.kind, other)?;
                    items.extend(members(other, call.heap)?);
                } else {
                    let start = position(arg(&call, 1)?, length, true)?;
                    let end = match call.args.get(2) {
                        Some(Value::Absent) | None => length,
                        Some(end) => position(end, length, true)?,
                    };
                    items = items[start..end.max(start)].to_vec();
                }
                match self.kind {
                    Kind::Tuple => Ok(Value::Tuple(items.into())),
                    _ => allocate(Kind::List, Object::List(items), &mut call),
                }
            }
            Op::Keys | Op::Values | Op::Entries => {
                let pairs: Vec<(Value, Value)> = match &xs {
                    Value::List(_) | Value::Tuple(_) => members(&xs, call.heap)?
                        .into_iter()
                        .enumerate()
                        .map(|(i, x)| (int(i), x))
                        .collect(),
                    _ => match snapshot(&xs, call.heap)? {
                        Object::Map(entries) => entries,
                        Object::Set(items) => items.into_iter().map(|x| (x.clone(), x)).collect(),
                        Object::Record(fields) => fields
                            .into_iter()
                            .map(|(name, x)| (Value::text(name), x))
                            .collect(),
                        _ => unreachable!(),
                    },
                };
                let items = pairs
                    .into_iter()
                    .map(|(key, value)| match self.op {
                        Op::Keys => key,
                        Op::Values => value,
                        _ => Value::Tuple(vec![key, value].into()),
                    })
                    .collect();
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
}
