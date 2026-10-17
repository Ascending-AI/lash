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
    Args, Formula, FunctionDefinition, FunctionName, FunctionRegistry, Implementation, Integer,
    KERNEL_VERSION, Name, NativeCall, NativeError, NativeFunction, Nursery, Operand, Param,
    RegistryError, Signature, TextView, Type, Value,
};

pub use json::{decode_number, parse_json, stringify_json};

pub(super) type Function = (FunctionDefinition, Arc<dyn NativeFunction>);

struct Native {
    function: fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
    /// From the required parameters to all of them (`K-FN-004`).
    arity: std::ops::RangeInclusive<usize>,
}

impl NativeFunction for Native {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        if !self.arity.contains(&call.args.len()) {
            return Err(raise("arity", "wrong argument count"));
        }
        (self.function)(call)
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
pub fn text_json() -> Vec<Function> {
    let mut functions = text::functions();
    functions.extend(bytes::functions());
    functions.extend(json::functions());
    functions.extend(classify::functions());
    functions.extend(format::functions());
    functions
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
    name: &str,
    params: &[(&str, Type)],
    result: Type,
    errors: &[&str],
    native: fn(NativeCall<'_, '_>) -> Result<Value, NativeError>,
) -> Function {
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
    (
        FunctionDefinition {
            kernel: KERNEL_VERSION,
            name,
            signature: Signature { params, result },
            errors: errors.iter().map(|kind| (*kind).to_owned()).collect(),
            charge,
            guard: None,
            implementation: Implementation::Native,
            native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
        },
        Arc::new(Native {
            function: native,
            arity,
        }),
    )
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
