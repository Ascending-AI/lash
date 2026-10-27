//! Text, bytes, JSON and neutral number formatting (`docs/kernel/library-text-json.md`).

mod bytes;
mod classify;
mod format;
mod json;
#[cfg(test)]
mod laws;
mod text;

use std::borrow::Cow;
use std::sync::Arc;

use lash_kernel_doc::{
    Args, Formula, FunctionDefinition, FunctionRegistry, Implementation, Integer, KERNEL_VERSION,
    NativeCall, NativeError, NativeFunction, Nursery, Operand, PreparedNative, RegistryError,
    TextView, Value,
};

use {
    lash_kernel_value::FunctionName, lash_kernel_value::Name, lash_kernel_value::Param,
    lash_kernel_value::Signature, lash_kernel_value::Type,
};

pub use json::{decode_number, parse_json, stringify_json};

pub(super) enum Selection<'a> {
    Definitions,
    Native(&'a str),
}
pub(super) struct Function {
    definition: Option<FunctionDefinition>,
    native: Option<Arc<dyn NativeFunction>>,
}
impl Function {
    fn charge(&mut self, make: impl FnOnce(Formula) -> Formula) {
        if let Some(definition) = &mut self.definition
            && let Implementation::Native { charge } | Implementation::Both { charge, .. } =
                &mut definition.implementation
        {
            *charge = make(std::mem::replace(charge, Formula::Constant(0)));
        }
    }
}

struct Native {
    function: fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
    /// From the required parameters to all of them (`K-FN-004`).
    arity: std::ops::RangeInclusive<usize>,
    /// Its entry over immediates, which answers what the general call
    /// answers where it needs no arena.
    prepared: Option<PreparedNative>,
}

impl NativeFunction for Native {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        if !self.arity.contains(&call.args.len()) {
            return Err(raise("arity", "wrong argument count"));
        }
        (self.function)(call)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        self.prepared
    }
}

/// The native text, bytes, JSON and format definitions, ready to register.
///
/// Definitions charge one plus the deep sizes of their arguments and result;
/// `format.decimal_parts` adds 64 units for bounded binary64 conversion;
/// `text.join_or_index` charges two list passes, separator size and twice result size;
/// `text.trim_set` bounds its membership scan with an input/set size product.
/// `json.classify` instead charges one plus the work encoded in its result,
/// so an early refusal never prices the unvisited input.
/// Unicode-dependent names include the pinned Unicode data version.
fn catalogue(selection: &Selection<'_>) -> Vec<Function> {
    let mut functions = text::functions(selection);
    functions.push(definition(
        selection,
        "timestamp.nanoseconds",
        &[("value", Type::Timestamp)],
        Type::Int,
        &[],
        timestamp_nanoseconds,
    ));
    functions.push(definition(
        selection,
        "timestamp.from_nanoseconds",
        &[("value", Type::Int)],
        Type::Timestamp,
        &[],
        timestamp_from_nanoseconds,
    ));
    functions.extend(bytes::functions(selection));
    functions.extend(json::functions(selection));
    functions.extend(classify::functions(selection));
    functions.extend(format::functions(selection));
    functions
}

pub fn text_json() -> Vec<(FunctionDefinition, Arc<dyn NativeFunction>)> {
    catalogue(&Selection::Definitions)
        .into_iter()
        .map(|function| match (function.definition, function.native) {
            (Some(definition), Some(native)) => (definition, native),
            _ => unreachable!("definition selection builds each definition and native"),
        })
        .collect()
}

/// Constructs one native from the same catalogue that authors its definition.
/// No definition or unrelated native is constructed on this path.
pub fn native(name: &str) -> Option<Arc<dyn NativeFunction>> {
    catalogue(&Selection::Native(name))
        .into_iter()
        .find_map(|function| function.native)
}

/// Adds each text, bytes, JSON and format native to the registered
/// definition of its name, which the build's release image holds
/// (FIG-6843).
///
/// # Errors
///
/// The first [`FunctionRegistry::implement_named`] refuses.
pub fn implement_text_json(registry: &mut FunctionRegistry) -> Result<(), RegistryError> {
    for (definition, native) in text_json() {
        registry.implement_named(definition.name.as_str(), native)?;
    }
    Ok(())
}

/// Adds the text, bytes, JSON and format functions to an embedder's registry.
pub fn register_text_json(registry: &mut FunctionRegistry) -> Result<(), RegistryError> {
    for (definition, native) in text_json() {
        registry.register(definition, Some(native))?;
    }
    Ok(())
}

/// A definition whose parameters are named as in kernel text: a name that
/// ends in `?` is an optional parameter.
fn definition(
    selection: &Selection<'_>,
    name: &str,
    params: &[(&str, Type)],
    result: Type,
    errors: &[&str],
    native: fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
) -> Function {
    prepared_definition(selection, name, params, result, errors, native, None)
}

/// [`definition`], its native answering first by `prepared` where that
/// entry's guard holds.
fn prepared_definition(
    selection: &Selection<'_>,
    name: &str,
    params: &[(&str, Type)],
    result: Type,
    errors: &[&str],
    native: fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
    prepared: Option<PreparedNative>,
) -> Function {
    if let Selection::Native(selected) = selection {
        return Function {
            definition: None,
            native: (*selected == name).then(|| {
                Arc::new(Native {
                    function: native,
                    arity: params
                        .iter()
                        .filter(|(name, _)| !name.ends_with('?'))
                        .count()..=params.len(),
                    prepared,
                }) as Arc<dyn NativeFunction>
            }),
        };
    }
    // All names are literals supplied by this module, or literals followed by
    // decimal Unicode-version components; none comes from guest input.
    let name = match FunctionName::new(name) {
        Ok(name) => name,
        Err(_) => unreachable!("library names are qualified identifiers"),
    };
    let params: Vec<_> = params
        .iter()
        .map(|(name, ty)| Param {
            name: Name::new(name.trim_end_matches('?')),
            ty: ty.clone(),
            optional: name.ends_with('?'),
        })
        .collect();
    let charge = Formula::Sum(
        std::iter::once(Formula::Constant(1))
            .chain(
                params
                    .iter()
                    .map(|param| Formula::DeepSize(Operand::Param(param.name.clone()))),
            )
            .chain(std::iter::once(Formula::DeepSize(Operand::Result)))
            .collect(),
    );
    let arity = params.iter().filter(|param| !param.optional).count()..=params.len();
    Function {
        definition: Some(FunctionDefinition {
            projection: Default::default(),
            kernel: KERNEL_VERSION,
            name,
            signature: Signature { params, result },
            errors: errors.iter().map(|kind| (*kind).to_owned()).collect(),
            guard: None,
            implementation: Implementation::Native { charge },
            native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
        }),
        native: Some(Arc::new(Native {
            function: native,
            arity,
            prepared,
        })),
    }
}

fn raise(kind: &'static str, message: impl Into<String>) -> NativeError {
    NativeError::refused(kind, message)
}

fn arg(args: &[Value], index: usize) -> Result<&Value, NativeError> {
    args.get(index)
        .ok_or_else(|| raise("arity", "missing argument"))
}

/// Text argument `index`.
fn text_view<'call>(args: &Args<'call>, index: usize) -> Result<TextView<'call>, NativeError> {
    args.text(arg(args.values(), index)?)?
        .ok_or_else(|| raise("type_error", "expected text"))
}

/// Text argument `index`, whole.
fn text_arg<'call>(args: &Args<'call>, index: usize) -> Result<Cow<'call, str>, NativeError> {
    text_view(args, index)?.whole()
}

/// Text argument `index`, whole, as the heap copied it out: a paged text
/// holds its bytes ([`Nursery::read_whole`]).
fn materialized_text<'call>(
    args: &Args<'call>,
    new: &mut Nursery<'_>,
    index: usize,
) -> Result<Cow<'call, str>, NativeError> {
    let text = text_view(args, index)?;
    new.read_whole(text.len())?;
    text.whole()
}

fn integer_value<'call>(
    args: &Args<'call>,
    index: usize,
) -> Result<Cow<'call, Integer>, NativeError> {
    args.integer(arg(args.values(), index)?)?
        .ok_or_else(|| raise("type_error", "expected integer"))
}

fn count_arg(args: &Args<'_>, index: usize) -> Result<usize, NativeError> {
    let count = match arg(args.values(), index)? {
        Value::Int(count) => usize::try_from(*count).ok(),
        Value::BigInt(_) => num_traits::ToPrimitive::to_usize(&*integer_value(args, index)?),
        _ => return Err(raise("type_error", "expected integer")),
    };
    count.ok_or_else(|| {
        raise(
            "number_range",
            "expected a nonnegative machine-sized integer",
        )
    })
}

/// An empty buffer with room for a text of `bytes`. The buffer and the text
/// it becomes are reserved against the run's memory bound first
/// ([`Nursery::reserve`]), so a text the bound has no room for is
/// refused before a byte of it is allocated.
fn text_buffer(new: &mut Nursery<'_>, bytes: usize) -> Result<String, NativeError> {
    new.reserve(0, room(bytes).saturating_mul(2))?;
    let mut buffer = String::new();
    buffer
        .try_reserve_exact(bytes)
        .map_err(|_| crate::exhausted())?;
    Ok(buffer)
}

/// Reserves a list of `count` values that hold `payload` bytes between
/// them, before the values are built.
fn reserve_list(new: &mut Nursery<'_>, count: usize, payload: usize) -> Result<(), NativeError> {
    new.reserve(room(count), room(payload))
}

fn room(bytes: usize) -> u64 {
    u64::try_from(bytes).unwrap_or(u64::MAX)
}

/// A count or an offset, which always fits a word.
fn int(value: impl TryInto<i64>) -> Value {
    Value::Int(value.try_into().unwrap_or(i64::MAX))
}

fn ordering(order: std::cmp::Ordering) -> Value {
    int(match order {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

/// The values of list argument `index`.
fn sequence(args: &Args<'_>, index: usize) -> Result<Vec<Value>, NativeError> {
    let list = arg(args.values(), index)?;
    if !matches!(list, Value::List(_)) {
        return Err(raise("type_error", "expected list"));
    }
    sequence_values(args, list)
}

/// The values of a list or a tuple.
fn sequence_values(args: &Args<'_>, value: &Value) -> Result<Vec<Value>, NativeError> {
    let Some(sequence) = args.sequence(value)? else {
        return Err(raise("type_error", "expected list"));
    };
    let mut items = Vec::with_capacity(sequence.len() as usize);
    for chunk in sequence.chunks() {
        items.extend_from_slice(chunk?);
    }
    Ok(items)
}

fn sequence_type(element: Type) -> Type {
    Type::List(Box::new(element))
}

fn timestamp_nanoseconds(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let value = arg(call.args.values(), 0)?;
    let nanoseconds = call
        .args
        .timestamp(value)?
        .ok_or_else(|| raise("type_error", "expected timestamp"))?;
    Ok(call.new.integer(nanoseconds.into_owned()))
}

fn timestamp_from_nanoseconds(call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
    let value = arg(call.args.values(), 0)?;
    let nanoseconds = call
        .args
        .integer(value)?
        .ok_or_else(|| raise("type_error", "expected integer"))?;
    Ok(call.new.timestamp(nanoseconds.into_owned()))
}
