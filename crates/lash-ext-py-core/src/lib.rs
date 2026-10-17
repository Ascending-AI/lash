//! The Python dialect's hot operations, as partial natives (FIG-6440).
//!
//! An operation is a dialect helper whose definition states a native
//! implementation beside its complete kernel body (`K-LIB-002`). Each native
//! here answers the inputs it can answer exactly as the body does and
//! declines the rest ([`NativeError::Decline`]), which the body then runs,
//! raising the operand errors itself. A native is a function of its
//! arguments (`K-LIB-006`). Where the body calls a kernel library function
//! for its answer, the native calls that function's own native, so values,
//! errors and reservations agree. The formula charges the operation
//! whichever path runs (`K-CHG-007`).
//!
//! The dialect crate defines the helpers and knows nothing of this crate;
//! an embedder adds each native to the helper its build registered
//! ([`implement`]).

use std::sync::Arc;

use lash_kernel_doc::heap::IntRef;
use lash_kernel_doc::{
    Float, FunctionId, FunctionRegistry, Integer, NativeCall, NativeError, NativeFunction,
    PreparedNative, RegistryError, Value,
};

/// Why [`implement`] could not add the natives.
#[derive(Debug, thiserror::Error)]
pub enum ImplementError {
    #[error(transparent)]
    Registry(#[from] RegistryError),
    /// A native here calls a kernel library function the registry holds no
    /// native implementation of.
    #[error("the kernel library function `{0}` has no native implementation registered")]
    Missing(&'static str),
}

/// The helpers this crate has a native for, by name.
pub const OPERATIONS: [&str; 2] = ["py.truth", "py.add"];

/// Adds its native to each of `helpers` that this crate implements
/// ([`OPERATIONS`]), each looked up by the name its definition carries.
/// The registry must hold the kernel library already: the natives call
/// its `add`, `text.concat` and `list.concat`.
///
/// # Errors
///
/// [`ImplementError::Missing`], and [`ImplementError::Registry`] when a
/// helper is not registered or its definition states no native.
pub fn implement(
    registry: &mut FunctionRegistry,
    helpers: impl IntoIterator<Item = FunctionId>,
) -> Result<(), ImplementError> {
    let kernel = |name: &'static str| {
        registry
            .native_named(name)
            .ok_or(ImplementError::Missing(name))
    };
    let add: Arc<dyn NativeFunction> = Arc::new(Add {
        add: kernel("add")?,
        text: kernel("text.concat")?,
        list: kernel("list.concat")?,
    });
    let mut natives: Vec<(FunctionId, Arc<dyn NativeFunction>)> = Vec::new();
    for function in helpers {
        let registered = registry
            .get(&function)
            .ok_or(RegistryError::NotRegistered { function })?;
        let native: Arc<dyn NativeFunction> = match registered.definition().name.as_str() {
            "py.truth" => Arc::new(Truth),
            "py.add" => Arc::clone(&add),
            _ => continue,
        };
        natives.push((function, native));
    }
    for (function, native) in natives {
        registry.implement(function, native)?;
    }
    Ok(())
}

/// Scalar truth needs no intermediate length value. Collection and tuple
/// truth declines to the body, which holds that length before comparing it.
struct Truth;

impl Truth {
    /// The truth of a value whose body holds no intermediate length.
    fn scalar(value: &Value) -> Option<bool> {
        Some(match value {
            Value::Bool(value) => *value,
            Value::Null => false,
            Value::Int(IntRef::Inline(value)) => value.bits() != 0,
            Value::Int(IntRef::Paged { summary, .. }) => summary.bits != 0,
            // `eq(value, 0)`, which NaN is not and -0 is.
            Value::Float(value) => value.get() != 0.0,
            Value::Text(text) => text.summary.utf8_bytes != 0,
            Value::Tuple(_) => return None,
            Value::List(_) | Value::Map(_) | Value::Record(_) | Value::Set(_) => return None,
            _ => true,
        })
    }
}

impl NativeFunction for Truth {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let value = call.args.first().unwrap_or(&Value::Absent);
        let truth = match Self::scalar(value) {
            Some(truth) => truth,
            None => return Err(NativeError::Decline),
        };
        Ok(Value::Bool(truth))
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            entry: |args| Some(Value::Bool(Self::scalar(args.first()?)?)),
            operation: "py.truth",
        })
    }
}

/// Two numbers add, and two texts or lists concatenate. Tuple addition
/// declines because its body releases argument-construction pins before
/// allocating the result. Unsupported pairs retain the body's `TypeError`.
struct Add {
    add: Arc<dyn NativeFunction>,
    text: Arc<dyn NativeFunction>,
    list: Arc<dyn NativeFunction>,
}

impl NativeFunction for Add {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        let (left, right) = match call.args {
            [left, right, ..] => (left, right),
            _ => return Err(NativeError::Decline),
        };
        let native = match (left, right) {
            (Value::Int(_) | Value::Float(_), Value::Int(_) | Value::Float(_)) => &self.add,
            (Value::Text(_), Value::Text(_)) => &self.text,
            (Value::List(_), Value::List(_)) => &self.list,
            // The body releases argument-construction pins before tuple
            // concatenation. An inline native retains those extra holdings.
            (Value::Tuple(_), Value::Tuple(_)) => return Err(NativeError::Decline),
            _ => return Err(NativeError::Decline),
        };
        native.call(call)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            entry: |args| match args {
                [Value::Float(left), Value::Float(right), ..] => {
                    Some(Value::Float(Float::new(left.get() + right.get())))
                }
                [
                    Value::Int(IntRef::Inline(left)),
                    Value::Int(IntRef::Inline(right)),
                    ..,
                ] => {
                    let sum = i64::try_from(&*left.as_bigint())
                        .ok()?
                        .checked_add(i64::try_from(&*right.as_bigint()).ok()?)?;
                    Some(Value::Int(IntRef::Inline(Integer::from(sum))))
                }
                _ => None,
            },
            operation: "py.add / scalar",
        })
    }
}
