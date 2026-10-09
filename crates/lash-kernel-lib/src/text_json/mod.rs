//! Text, bytes, JSON and neutral number formatting (`docs/kernel/library-text-json.md`).

mod bytes;
mod format;
mod json;
mod text;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use lash_kernel_doc::{
    ErrorValue, Formula, FunctionDefinition, FunctionName, FunctionRegistry, Implementation,
    KERNEL_VERSION, Name, NativeCall, NativeError, NativeFunction, Operand, Param, RegistryError,
    Signature, Type, Value,
};
use num_traits::ToPrimitive;

pub use json::{decode_number, parse_json, stringify_json};

pub(super) type Function = (FunctionDefinition, Arc<dyn NativeFunction>);

struct Native {
    function: fn(NativeCall<'_>) -> Result<Value, NativeError>,
    arity: usize,
}

impl NativeFunction for Native {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        if call.args.len() != self.arity {
            return Err(raise("arity", "wrong argument count"));
        }
        (self.function)(call)
    }
}

/// The native text, bytes, JSON and format definitions, ready to register.
///
/// Each definition charges one plus the deep sizes of its arguments and result.
/// Unicode-dependent names include the pinned Unicode data version.
pub fn text_json() -> Vec<Function> {
    let mut functions = text::functions();
    functions.extend(bytes::functions());
    functions.extend(json::functions());
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

fn definition(
    name: &str,
    params: &[(&str, Type)],
    result: Type,
    errors: &[&str],
    native: fn(NativeCall<'_>) -> Result<Value, NativeError>,
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
            name: Name::new(*name),
            ty: ty.clone(),
            optional: false,
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
    let arity = params.len();
    (
        FunctionDefinition {
            kernel: KERNEL_VERSION,
            name,
            signature: Signature { params, result },
            errors: errors.iter().map(|kind| (*kind).to_owned()).collect(),
            charge,
            guard: None,
            implementation: Implementation::Native,
        },
        Arc::new(Native {
            function: native,
            arity,
        }),
    )
}

fn raise(kind: &str, message: &str) -> NativeError {
    NativeError::Raised(ErrorValue::new(kind, message))
}

fn arg<'a>(call: &'a NativeCall<'_>, index: usize) -> Result<&'a Value, NativeError> {
    call.args
        .get(index)
        .ok_or_else(|| raise("arity", "missing argument"))
}

fn text_arg<'a>(call: &'a NativeCall<'_>, index: usize) -> Result<&'a str, NativeError> {
    match arg(call, index)? {
        Value::Text(text) => Ok(text),
        _ => Err(raise("type_error", "expected text")),
    }
}

fn integer_arg<'a>(
    call: &'a NativeCall<'_>,
    index: usize,
) -> Result<&'a num_bigint::BigInt, NativeError> {
    match arg(call, index)? {
        Value::Int(integer) => Ok(integer.as_bigint()),
        _ => Err(raise("type_error", "expected integer")),
    }
}

fn count_arg(call: &NativeCall<'_>, index: usize) -> Result<usize, NativeError> {
    integer_arg(call, index)?.to_usize().ok_or_else(|| {
        raise(
            "number_range",
            "expected a nonnegative machine-sized integer",
        )
    })
}

fn int(value: impl Into<num_bigint::BigInt>) -> Value {
    Value::Int(lash_kernel_doc::Integer::new(value))
}

fn ordering(order: std::cmp::Ordering) -> Value {
    int(match order {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    })
}

fn sequence(call: &NativeCall<'_>, index: usize) -> Result<Vec<Value>, NativeError> {
    match arg(call, index)? {
        Value::List(id) => (0..call.heap.len(*id))
            .map(|i| {
                call.heap
                    .list_get(*id, i)
                    .ok_or_else(|| raise("type_error", "invalid list"))
            })
            .collect(),
        _ => Err(raise("type_error", "expected list")),
    }
}

fn sequence_type(element: Type) -> Type {
    Type::List(Box::new(element))
}
