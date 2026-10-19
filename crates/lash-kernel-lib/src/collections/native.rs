use std::sync::Arc;

use lash_kernel_doc::{
    Args, Float, Integer, NativeCall, NativeError, NativeFunction, NewObject, PreparedNative, Value,
};
use num_bigint::BigInt;
use num_traits::{FromPrimitive, ToPrimitive};

use super::{CollectionError, copy, raise};
use crate::numbers::KIND_NAMES;

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
                    charged_entry: None,
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
    // Each merge pass compares a pair at most once per element it places,
    // and a comparison reads no further than the shallower of its keys.
    let definition = define("list.sort_by_keys", &|| {
        String::from(
            "function list.sort_by_keys(items: List(Any), keys: List(Any), descending: Bool) -> List(Any)\nkernel 1\nerrors \"type_error\"\ncharge sum(1, size(items), product(2, sum(size(keys), deep(keys)), sum(1, ceil_log2(size(keys)))), size(result))\nnative\n",
        )
    })?;
    functions.push((definition, Arc::new(SortByKeys) as _));
    let prepared = |function: fn(&[Value]) -> Result<Value, NativeError>,
                    entry: fn(&[Value]) -> Option<Value>,
                    operation: &'static str|
     -> Arc<dyn NativeFunction> {
        Arc::new(Aux(
            function,
            Some(PreparedNative {
                charged_entry: None,
                entry,
                operation,
            }),
        ))
    };
    for (signature, native, charge) in [
        (
            "bool.not(x: Bool) -> Bool",
            prepared(
                bool_not,
                |args| bool_not(args).ok(),
                "bool.not(x: Bool) -> Bool",
            ),
            "1",
        ),
        (
            "error.new(kind: Text, message: Text, data?: Any) -> Error",
            // Its value is built in the nursery: it has no prepared entry.
            Arc::new(ErrorNew) as Arc<dyn NativeFunction>,
            "sum(1, size(kind), size(message), size(result))",
        ),
        (
            "collection.function_check(x: Any) -> Any",
            prepared(
                function_check,
                |args| function_check(args).ok(),
                "collection.function_check(x: Any) -> Any",
            ),
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
        functions.push((definition, native));
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
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let args = call.args;
        let kinds = nth(args.values(), 1)?;
        let (Value::Set(_), Some(table)) = (kinds, args.table(kinds)?) else {
            return Err(raise("type_error", "kinds must be a set of kind names"));
        };
        let mut refused = [false; KIND_NAMES.len()];
        for chunk in table.chunks() {
            for pair in chunk?.as_chunks::<2>().0.iter() {
                let named = match args.text(&pair[0])? {
                    Some(name) => {
                        let name = name.whole()?;
                        KIND_NAMES.iter().position(|(_, kind)| *kind == &*name)
                    }
                    None => None,
                };
                match named {
                    Some(index) => refused[index] = true,
                    None => return Err(raise("type_error", "kinds must be a set of kind names")),
                }
            }
        }
        copy::holds_none(nth(args.values(), 0)?, &refused, &args, call.new).map(Value::Bool)
    }
}
/// A function of its arguments alone.
struct Aux(
    fn(&[Value]) -> Result<Value, NativeError>,
    Option<PreparedNative>,
);
impl NativeFunction for Aux {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        (self.0)(call.args.values())
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
/// `error.new(kind, message, data?)`.
struct ErrorNew;
impl NativeFunction for ErrorNew {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let values = call.args.values();
        let kind = *field(nth(values, 0)?)?;
        let message = *field(nth(values, 1)?)?;
        let data = values.get(2).copied().unwrap_or(Value::Absent);
        Ok(call.new.allocate(NewObject::Error {
            kind,
            message,
            data,
        }))
    }
}
fn function_check(args: &[Value]) -> Result<Value, NativeError> {
    match nth(args, 0)? {
        value @ (Value::Closure(_) | Value::Function(_) | Value::LongFunction(_)) => Ok(*value),
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
        value @ (Value::Int(_) | Value::BigInt(_)) => Ok(*value),
        _ => Err(raise("type_error", "range arguments must be integers")),
    }
}

fn object(kind: Kind, value: &Value) -> Result<(), NativeError> {
    match (kind, value) {
        (Kind::List, Value::List(_))
        | (Kind::Map, Value::Map(_))
        | (Kind::Set, Value::Set(_))
        | (Kind::Record, Value::Record(_))
        | (Kind::Tuple, Value::Tuple(_)) => Ok(()),
        _ => Err(raise("type_error", "collection has the wrong kind")),
    }
}
fn nth(args: &[Value], index: usize) -> Result<&Value, NativeError> {
    args.get(index)
        .ok_or_else(|| raise("arity", "missing collection argument"))
}
/// An integral index: a wide word, or the sign of an integer past one.
#[derive(Clone, Copy)]
enum Index {
    Word(i128),
    Big { negative: bool },
}
impl Index {
    fn negative(self) -> bool {
        match self {
            Self::Word(n) => n < 0,
            Self::Big { negative } => negative,
        }
    }
    fn machine(self) -> Option<usize> {
        match self {
            Self::Word(n) => usize::try_from(n).ok(),
            Self::Big { .. } => None,
        }
    }
    fn of(n: &Integer) -> Self {
        match n.to_i128() {
            Some(n) => Self::Word(n),
            None => Self::Big {
                negative: n.is_negative(),
            },
        }
    }
}
fn integer(args: &Args<'_>, value: &Value) -> Result<Index, NativeError> {
    match value {
        Value::Int(n) => Ok(Index::Word(i128::from(*n))),
        Value::BigInt(_) => match args.integer(value)? {
            Some(n) => Ok(Index::of(&n)),
            None => Err(raise("type_error", "index must be a number")),
        },
        Value::Float(n) if n.get().is_finite() && n.get().fract() == 0.0 => {
            BigInt::from_f64(n.get())
                .map(|n| Index::of(&Integer::new(n)))
                .ok_or_else(|| raise("index_out_of_range", "index must be integral"))
        }
        Value::Float(_) => Err(raise("index_out_of_range", "index must be integral")),
        _ => Err(raise("type_error", "index must be a number")),
    }
}
fn int(n: usize) -> Value {
    Value::Int(n as i64)
}
fn position(value: Index, length: usize, clamp: bool) -> Result<usize, NativeError> {
    if let Index::Word(mut n) = value {
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
        return Ok(if value.negative() { 0 } else { length });
    }
    Err(raise(
        "index_out_of_range",
        "index is outside the collection",
    ))
}
fn key(value: &Value, args: &Args<'_>) -> Result<(), NativeError> {
    // An inline scalar or short text is a legal key by its kind: its
    // normal form need not be built to be known.
    if matches!(
        value,
        Value::Null
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Text(_)
            | Value::Timestamp(_)
            | Value::Function(_)
            | Value::TaskRef(_)
    ) {
        return Ok(());
    }
    if matches!(value, Value::Tuple(_))
        && args.depth(value) as usize > args.limits().value_depth().min(128)
    {
        return Err(raise("too_deep", "key exceeds the configured value depth"));
    }
    args.normal_key(value)?
        .ok_or_else(|| raise("invalid_key", "expected an immutable key or explicit ref"))?;
    Ok(())
}
fn field(value: &Value) -> Result<&Value, NativeError> {
    match value {
        Value::Text(_) | Value::LongText(_) => Ok(value),
        _ => Err(raise("type_error", "field name must be text")),
    }
}

/// How many values a list or a tuple holds, or entries a table.
fn length(args: &Args<'_>, xs: &Value) -> Result<usize, NativeError> {
    if let Some(sequence) = args.sequence(xs)? {
        return Ok(sequence.len() as usize);
    }
    if let Some(table) = args.table(xs)? {
        return Ok(table.len() as usize);
    }
    Err(raise("type_error", "expected a collection"))
}
/// The values of a list or a tuple, in order.
pub(super) fn items(args: &Args<'_>, xs: &Value) -> Result<Vec<Value>, NativeError> {
    let Some(sequence) = args.sequence(xs)? else {
        return Err(raise("type_error", "expected a sequence"));
    };
    let mut items = Vec::with_capacity(sequence.len() as usize);
    for chunk in sequence.chunks() {
        items.extend_from_slice(chunk?);
    }
    Ok(items)
}
/// The entries of a map, a set or a record, as key and value pairs.
pub(super) fn entries(args: &Args<'_>, xs: &Value) -> Result<Vec<(Value, Value)>, NativeError> {
    let Some(table) = args.table(xs)? else {
        return Err(raise("type_error", "expected a table"));
    };
    let mut entries = Vec::with_capacity(table.len() as usize);
    for chunk in table.chunks() {
        entries.extend(
            chunk?
                .as_chunks::<2>()
                .0
                .iter()
                .map(|pair| (pair[0], pair[1])),
        );
    }
    Ok(entries)
}
/// A collection's contents in the shape it is built from.
pub(super) fn snapshot(args: &Args<'_>, value: &Value) -> Result<NewObject, NativeError> {
    match value {
        Value::List(_) => Ok(NewObject::List(items(args, value)?)),
        Value::Map(_) => Ok(NewObject::Map(entries(args, value)?)),
        Value::Set(_) => Ok(NewObject::Set(
            entries(args, value)?.into_iter().map(|(k, _)| k).collect(),
        )),
        Value::Record(_) => Ok(NewObject::Record(entries(args, value)?)),
        _ => Err(raise("type_error", "expected a collection")),
    }
}
fn append_range(
    args: &Args<'_>,
    value: &Value,
    start: usize,
    end: usize,
    items: &mut Vec<Value>,
) -> Result<(), NativeError> {
    let Some(sequence) = args.sequence(value)? else {
        return Err(NativeError::Malformed("sequence has another kind"));
    };
    for index in start..end {
        items.push(*sequence.get(index as u64)?.ok_or(NativeError::Malformed(
            "an admitted sequence member is absent",
        ))?);
    }
    Ok(())
}
fn result_items(
    call: &mut NativeCall<'_, '_>,
    count: usize,
    values_per_item: u64,
) -> Result<Vec<Value>, NativeError> {
    call.new
        .reserve((count as u64).saturating_mul(values_per_item), 0)?;
    let mut items = Vec::new();
    items.try_reserve_exact(count).map_err(|_| {
        NativeError::from(lash_kernel_doc::Retry::Room {
            wanted: lash_kernel_doc::heap::LogicalBytes(u64::MAX),
        })
    })?;
    Ok(items)
}
impl NativeFunction for Native {
    fn call(&self, mut call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let args = call.args;
        if let Some(result) = self.read(&args) {
            return result;
        }
        let values = args.values();
        let xs = *nth(values, 0)?;
        object(self.kind, &xs)?;
        let length = length(&args, &xs)?;
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
                    let object = snapshot(&args, &xs)?;
                    Ok(call.new.allocate(object))
                }
            },
            Op::DeepCopy => copy::deep(&xs, &args, call.new),
            Op::Slice | Op::Concat => {
                let items = if matches!(self.op, Op::Concat) {
                    let other = nth(values, 1)?;
                    object(self.kind, other)?;
                    let other_length = self::length(&args, other)?;
                    let count = length.checked_add(other_length).ok_or(NativeError::from(
                        lash_kernel_doc::Retry::Room {
                            wanted: lash_kernel_doc::heap::LogicalBytes(u64::MAX),
                        },
                    ))?;
                    let mut items = result_items(&mut call, count, 1)?;
                    append_range(&args, &xs, 0, length, &mut items)?;
                    append_range(&args, other, 0, other_length, &mut items)?;
                    items
                } else {
                    let start = position(integer(&args, nth(values, 1)?)?, length, true)?;
                    let end = match values.get(2) {
                        Some(Value::Absent) | None => length,
                        Some(end) => position(integer(&args, end)?, length, true)?,
                    }
                    .max(start);
                    let mut items = result_items(&mut call, end - start, 1)?;
                    append_range(&args, &xs, start, end, &mut items)?;
                    items
                };
                Ok(call.new.allocate(match self.kind {
                    Kind::Tuple => NewObject::Tuple(items),
                    _ => NewObject::List(items),
                }))
            }
            Op::Keys | Op::Values | Op::Entries => {
                let mut items = result_items(
                    &mut call,
                    length,
                    if matches!(self.op, Op::Entries) { 3 } else { 1 },
                )?;
                if matches!(self.kind, Kind::List | Kind::Tuple) {
                    match self.op {
                        Op::Keys => items.extend((0..length).map(int)),
                        Op::Values => items.extend(self::items(&args, &xs)?),
                        _ => {
                            for (index, value) in self::items(&args, &xs)?.into_iter().enumerate() {
                                let pair =
                                    call.new.allocate(NewObject::Tuple(vec![int(index), value]));
                                items.push(pair);
                            }
                        }
                    }
                } else {
                    let set = matches!(self.kind, Kind::Set);
                    for (key, value) in entries(&args, &xs)? {
                        let value = if set { key } else { value };
                        items.push(match self.op {
                            Op::Keys => key,
                            Op::Values => value,
                            _ => call.new.allocate(NewObject::Tuple(vec![key, value])),
                        });
                    }
                }
                Ok(call.new.allocate(NewObject::List(items)))
            }
            Op::With | Op::Without => {
                let name = *field(nth(values, 1)?)?;
                let mut fields = entries(&args, &xs)?;
                let mut at = None;
                for (index, (key, _)) in fields.iter().enumerate() {
                    if args.equal(key, &name)? {
                        at = Some(index);
                        break;
                    }
                }
                if matches!(self.op, Op::Without) {
                    if let Some(at) = at {
                        fields.remove(at);
                    }
                } else {
                    let value = *nth(values, 2)?;
                    match at {
                        Some(at) => fields[at].1 = value,
                        None => fields.push((name, value)),
                    }
                }
                Ok(call.new.allocate(NewObject::Record(fields)))
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
                    charged_entry: None,
                    operation: concat!(stringify!($kind), ".check"),
                    entry: |args| {
                        let value = args.first()?;
                        object(Kind::$kind, value).ok()?;
                        Some(*value)
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
    /// The call of an operation that only reads the arena: it allocates
    /// and reserves nothing. `None` for an operation that does.
    fn read(&self, args: &Args<'_>) -> Option<Result<Value, NativeError>> {
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
        .then(|| self.reading(args))
    }

    fn reading(&self, args: &Args<'_>) -> Result<Value, NativeError> {
        let values = args.values();
        let xs = nth(values, 0)?;
        object(self.kind, xs)?;
        let length = length(args, xs)?;
        match self.op {
            Op::Len => Ok(int(length)),
            Op::Check => Ok(*xs),
            Op::InsertIndex => {
                let n = integer(args, nth(values, 1)?)?;
                match n.machine() {
                    Some(at) if at <= length => Ok(Value::Int(at as i64)),
                    _ => Err(raise(
                        "index_out_of_range",
                        "insert position is outside the collection",
                    )),
                }
            }
            Op::Get | Op::GetOr | Op::At => match xs {
                Value::List(_) | Value::Tuple(_) => {
                    let Some(sequence) = args.sequence(xs)? else {
                        return Err(NativeError::Malformed("a sequence names another body"));
                    };
                    let index = integer(args, nth(values, 1)?)?;
                    if matches!(self.op, Op::Get | Op::GetOr) && index.negative() {
                        return Err(raise(
                            "index_out_of_range",
                            "get requires a nonnegative index",
                        ));
                    }
                    if matches!(self.op, Op::GetOr) && matches!(xs, Value::List(_)) {
                        return Ok(match index.machine() {
                            Some(i) => sequence.get(i as u64)?.copied(),
                            None => None,
                        }
                        .unwrap_or_else(|| values.get(2).copied().unwrap_or(Value::Absent)));
                    }
                    let i = position(index, length, false)?;
                    sequence.get(i as u64)?.copied().ok_or_else(|| {
                        raise(
                            "index_out_of_range",
                            if matches!(xs, Value::List(_)) {
                                "missing list element"
                            } else {
                                "missing tuple element"
                            },
                        )
                    })
                }
                Value::Map(_) => {
                    let k = nth(values, 1)?;
                    key(k, args)?;
                    lookup(args, xs, k)?.ok_or_else(|| raise("key_missing", "map has no such key"))
                }
                Value::Record(_) => {
                    Ok(lookup(args, xs, field(nth(values, 1)?)?)?.unwrap_or(Value::Absent))
                }
                _ => Err(raise("type_error", "collection has no get")),
            },
            Op::Contains | Op::IndexOf => {
                let value = nth(values, 1)?;
                let found = match xs {
                    Value::Map(_) | Value::Set(_) => {
                        key(value, args)?;
                        Some(lookup(args, xs, value)?.is_some())
                    }
                    Value::Record(_) => Some(lookup(args, xs, field(value)?)?.is_some()),
                    _ => None,
                };
                if let Some(found) = found {
                    return Ok(Value::Bool(found));
                }
                let Some(sequence) = args.sequence(xs)? else {
                    return Err(NativeError::Malformed("sequence has another kind"));
                };
                let mut found = None;
                let mut index = 0;
                'search: for chunk in sequence.chunks() {
                    for item in chunk? {
                        if args.equal(item, value)? {
                            found = Some(index);
                            break 'search;
                        }
                        index += 1;
                    }
                }
                Ok(match self.op {
                    Op::IndexOf => found.map_or(Value::Int(-1), int),
                    _ => Value::Bool(found.is_some()),
                })
            }
            _ => unreachable!("only an operation that reads is read"),
        }
    }
}

/// The value the table `xs` holds under `key`.
fn lookup(args: &Args<'_>, xs: &Value, key: &Value) -> Result<Option<Value>, NativeError> {
    let Some(table) = args.table(xs)? else {
        return Err(NativeError::Malformed("a table names another body"));
    };
    Ok(args.lookup(table, key)?.copied())
}

/// Searches an absolute half-open range using kernel equality, optionally
/// equating floating NaNs. Reverse selects the last match in the range.
/// `list.sort_by_keys(items, keys, descending)`: `items` in the order of
/// their `keys` by `lt`, stably, or by its reverse when `descending`. Keys
/// are data a caller computed once per item, so no function crosses into
/// the native (K-LIB-003). It merges as `list.sort` does under the
/// comparator `fn(a, b) { if lt(key(b), key(a)) { return 1 } return 0 }`
/// (`lt(key(a), key(b))` when descending): the same keys compared in the
/// same order, so keys with no shared ordering raise exactly where that
/// sort raises (FIG-6886).
struct SortByKeys;
impl NativeFunction for SortByKeys {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let args = call.args;
        let values = args.values();
        let (listed @ Value::List(_), ranks @ Value::List(_), Value::Bool(descending)) =
            (nth(values, 0)?, nth(values, 1)?, nth(values, 2)?)
        else {
            return Err(raise(
                "type_error",
                "sort_by_keys takes two lists and a bool",
            ));
        };
        let listed = items(&args, listed)?;
        let ranks = items(&args, ranks)?;
        if listed.len() != ranks.len() {
            return Err(raise("type_error", "every item needs exactly one key"));
        }
        let n = listed.len();
        let mut sorted: Vec<usize> = (0..n).collect();
        let mut out = vec![0; n];
        let mut width = 1;
        while width < n {
            let mut start = 0;
            while start < n {
                let mid = (start + width).min(n);
                let end = (mid + width).min(n);
                let (mut left, mut right, mut dest) = (start, mid, start);
                while left < mid && right < end {
                    let (before, after) = (sorted[left], sorted[right]);
                    let (first, second) = if *descending {
                        (before, after)
                    } else {
                        (after, before)
                    };
                    let order =
                        crate::comparison::compare(&ranks[first], &ranks[second], &args, call.new)?;
                    if order == Some(std::cmp::Ordering::Less) {
                        out[dest] = after;
                        right += 1;
                    } else {
                        out[dest] = before;
                        left += 1;
                    }
                    dest += 1;
                }
                out[dest..dest + (mid - left)].copy_from_slice(&sorted[left..mid]);
                dest += mid - left;
                out[dest..dest + (end - right)].copy_from_slice(&sorted[right..end]);
                start = end;
            }
            std::mem::swap(&mut sorted, &mut out);
            width *= 2;
        }
        Ok(call.new.allocate(NewObject::List(
            sorted.into_iter().map(|at| listed[at]).collect(),
        )))
    }
}
struct IndexOfRange;
impl NativeFunction for IndexOfRange {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        index_of_range(call)
    }
}
fn index_of_range(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let args = call.args;
    let values = args.values();
    let xs = nth(values, 0)?;
    let (Value::List(_), Some(sequence)) = (xs, args.sequence(xs)?) else {
        return Err(raise("type_error", "expected list"));
    };
    let needle = nth(values, 1)?;
    let length = sequence.len() as usize;
    let (Some(start), Some(end)) = (
        integer(&args, nth(values, 2)?)?.machine(),
        integer(&args, nth(values, 3)?)?.machine(),
    ) else {
        return Err(raise(
            "index_out_of_range",
            "range requires nonnegative indices",
        ));
    };
    if start > end || end > length {
        return Err(raise("index_out_of_range", "range is outside the list"));
    }
    let (Value::Bool(reverse), Value::Bool(nan_equal)) = (nth(values, 4)?, nth(values, 5)?) else {
        return Err(raise("type_error", "search options require booleans"));
    };
    let mut found = None;
    for at in 0..end - start {
        let index = if *reverse { end - at - 1 } else { start + at };
        let value = sequence
            .get(index as u64)?
            .ok_or_else(|| raise("index_out_of_range", "missing list element"))?;
        if args.equal(value, needle)?
            || (*nan_equal
                && matches!((value, needle), (Value::Float(a), Value::Float(b)) if a.get().is_nan() && b.get().is_nan()))
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
    call.new.reserve(0, bytes)?;
    Ok(found.map_or(Value::Int(-1), int))
}

/// K-COL-017: project immutable tuple structure, never object contents.
/// Encodings for absent and errors are supplied by the caller; no source
/// dialect's tokens or kind names are recognized here.
struct IdentityKey;
impl NativeFunction for IdentityKey {
    fn call(&self, mut call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let values = call.args.values();
        let value = *nth(values, 0)?;
        let absent = *nth(values, 1)?;
        let tag = *nth(values, 2)?;
        field(&tag)?;
        project_identity(&value, &absent, &tag, 0, &mut call)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            charged_entry: None,
            operation: "collection.identity_key(Any, Any, Text)",
            entry: |args| {
                field(args.get(2)?).ok()?;
                scalar_identity(args.first()?, args.get(1)?)
            },
        })
    }
}

/// A ref to the object or the task `value` is.
fn reference(value: &Value) -> Option<Value> {
    match value {
        Value::Task(task) => Some(Value::TaskRef(*task)),
        Value::List(object)
        | Value::Map(object)
        | Value::Set(object)
        | Value::Record(object)
        | Value::Closure(object) => Some(Value::ObjectRef(*object)),
        _ => None,
    }
}

fn scalar_identity(value: &Value, absent: &Value) -> Option<Value> {
    Some(match value {
        Value::Tuple(_) | Value::Error(_) => return None,
        Value::Absent => *absent,
        Value::Float(n) if n.get() == 0.0 => Value::Float(Float::new(0.0)),
        other => reference(other).unwrap_or(*other),
    })
}

fn project_identity(
    value: &Value,
    absent: &Value,
    tag: &Value,
    depth: usize,
    call: &mut NativeCall<'_, '_>,
) -> Result<Value, NativeError> {
    let args = call.args;
    if depth > args.limits().value_depth().min(128) {
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
        Value::Tuple(_) => {
            let members = items(&args, value)?;
            call.new.reserve(members.len() as u64, 0)?;
            let mut projected = Vec::with_capacity(members.len());
            for item in &members {
                projected.push(project_identity(item, absent, tag, depth + 1, call)?);
            }
            Ok(call.new.allocate(NewObject::Tuple(projected)))
        }
        Value::Error(_) => {
            let Some(error) = args.error(value)? else {
                return Err(NativeError::Malformed("an error names another body"));
            };
            let data = reference(&error.data)
                .ok_or_else(|| raise("type_error", "ref requires an object or task"))?;
            call.new.reserve(4, 0)?;
            Ok(call.new.allocate(NewObject::Tuple(vec![
                *tag,
                data,
                error.kind,
                error.message,
            ])))
        }
        _ => unreachable!("scalar identity covered other values"),
    }
}
