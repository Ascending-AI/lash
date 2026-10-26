//! The TypeScript dialect's helpers: closed ones as Rust plans compiled from
//! their kernel bodies (FIG-6678), and the hot operations that can reach
//! guest code as partial natives (FIG-6440), plus total numeric indexing.
//!
//! The helper compiler (`lash-ext-helper-compiler`) compiles each closed body
//! to a plan the machine runs where a native would answer, through the
//! machine's own primitives; the body stays what forced-fallback layouts
//! and a restored frame run. The plans are generated sources, checked in
//! beside their manifest (`generated/plans.tsv`), and a test here fails when they
//! are not what the compiler gives for the current bodies.
//!
//! An operation is a dialect helper whose definition states a native
//! implementation beside its complete kernel body (`K-LIB-002`). Each native
//! here answers the inputs it can answer exactly as the body does and
//! declines the rest ([`NativeError::Decline`]), which the body then runs: a
//! conversion that may run guest code (`valueOf`, `toString`) is always the
//! body's. A native is a function of its arguments (`K-LIB-006`). Where the
//! body calls a kernel library function for its answer, the native calls
//! that function's own native, so values, errors and reservations agree.
//! The formula charges the operation whichever path runs (`K-CHG-007`).
//!
//! The dialect crate defines the helpers and knows nothing of this crate;
//! an embedder installs each plan and adds each native beside the helper
//! its build registered ([`implement`]).

mod conversions;
mod number_index;

use std::sync::Arc;

use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::NativeCall;
use lash_kernel_doc::NativeError;
use lash_kernel_doc::NativeFunction;
use lash_kernel_doc::PreparedNative;
use lash_kernel_doc::RegistryError;
use lash_kernel_doc::Value;
use lash_kernel_value::names::FunctionId;

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

#[rustfmt::skip]
#[path = "generated/plans.rs"]
mod plans;

pub use plans::PLANS;

/// Installs each plan of [`PLANS`] beside the helper of `helpers` it was
/// compiled from, then adds its native to each of `helpers` that this
/// crate implements ([`OPERATIONS`]), each looked up by the name its
/// definition carries. Closed kernel callee plans are installed as
/// dependencies, sharing an already installed kernel plan across dialects.
/// The registry must hold the kernel library already: the natives call
/// its `num.sub`, `num.lt` and `text.utf16_compare`.
///
/// # Errors
///
/// [`ImplementError::Missing`], and [`ImplementError::Registry`] when a
/// helper is not registered or its definition states no native, or a plan
/// calls a body the registry answers with neither a plan nor a native.
pub fn implement(
    registry: &mut FunctionRegistry,
    helpers: impl IntoIterator<Item = FunctionId>,
) -> Result<(), ImplementError> {
    let helpers: std::collections::BTreeSet<FunctionId> = helpers.into_iter().collect();
    let plans: Vec<&'static lash_kernel_doc::plan::HelperPlan> = PLANS
        .iter()
        .copied()
        .filter(|plan| {
            helpers.contains(&plan.function)
                || !plan.name.starts_with("py.")
                    && !plan.name.starts_with("ts.")
                    && registry
                        .get(&plan.function)
                        .is_some_and(|registered| registered.plan().is_none())
        })
        .collect();
    registry.install_plans(&plans)?;
    for name in ["num.sub", "num.lt", "text.utf16_compare"] {
        registry
            .native_named(name)
            .ok_or(ImplementError::Missing(name))?;
    }
    for function in helpers {
        let registered = registry
            .get(&function)
            .ok_or(RegistryError::NotRegistered { function })?;
        let listed = registered.listed();
        if let Some(native) = native(registered.name(), |name| {
            let (callee, _) = listed.iter().find(|(_, label)| label.as_str() == name)?;
            registry.get(callee)?.native().cloned()
        }) {
            registry.implement(function, native)?;
        }
    }
    Ok(())
}

// One declaration owns both the constructor dispatch and its name projection.
macro_rules! natives {
    ($name:ident, $kernel:ident; $( $($label:literal)|+ => $implementation:expr ),+ $(,)?) => {
        /// The helpers this crate has a hand-written native for, by name.
        pub const OPERATIONS: [&str; [$($(stringify!($label)),+),+].len()] = [$($($label),+),+];

        /// Constructs a native by the build's identity table, reading only the
        /// pure kernel implementations that this native delegates to.
        pub fn native(
            $name: &str,
            $kernel: impl Fn(&str) -> Option<Arc<dyn NativeFunction>>,
        ) -> Option<Arc<dyn NativeFunction>> {
            Some(match $name {
                $( $($label)|+ => $implementation, )+
                _ => return None,
            })
        }
    };
}

natives!(name, kernel;
        "ts.builtin_name" => Arc::new(BuiltinName),
        "ts.sub" => Arc::new(Subtract {
            native: kernel("num.sub")?,
        }),
        "ts.to_number" => Arc::new(ToNumber),
        "ts.to_boolean" => Arc::new(conversions::ToBoolean),
        "ts.number.format_value" => Arc::new(conversions::NumberToString),
        "ts.number_index" => Arc::new(number_index::NumberIndex {
            less: kernel("num.lt")?,
        }),
        "ts.lt" | "ts.le" | "ts.gt" | "ts.ge" => Arc::new(Relate {
            relation: match name {
                "ts.lt" => Relation::Lt,
                "ts.le" => Relation::Le,
                "ts.gt" => Relation::Gt,
                _ => Relation::Ge,
            },
            compare: kernel("text.utf16_compare")?,
        }),
);

fn pair<'a>(call: &'a NativeCall<'_, '_>) -> (&'a Value, &'a Value) {
    match call.args.values() {
        [left, right, ..] => (left, right),
        [left] => (left, &Value::Absent),
        [] => (&Value::Absent, &Value::Absent),
    }
}

#[derive(Clone, Copy)]
enum Relation {
    Lt,
    Le,
    Gt,
    Ge,
}

impl Relation {
    /// The relation of two floats, as `num.lt` and `num.le` order them:
    /// NaN is unordered, and -0 equals 0.
    fn floats(self, left: f64, right: f64) -> bool {
        match self {
            Self::Lt => left < right,
            Self::Le => left <= right,
            Self::Gt => left > right,
            Self::Ge => left >= right,
        }
    }
}

/// `ts.lt`, `ts.le`, `ts.gt` and `ts.ge` on two floats, or on two texts by
/// UTF-16 code units, as the body compares them after ToPrimitive leaves
/// both as they are. Every other pair declines.
struct Relate {
    relation: Relation,
    compare: Arc<dyn NativeFunction>,
}

impl NativeFunction for Relate {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match pair(&call) {
            (Value::Float(a), Value::Float(b)) => {
                Ok(Value::Bool(self.relation.floats(a.get(), b.get())))
            }
            (Value::Text(_) | Value::LongText(_), Value::Text(_) | Value::LongText(_)) => {
                // The body orders the texts as each relation states; the
                // order of the call's two arguments answers every relation.
                let order = match self.compare.call(call)? {
                    Value::Int(order) => order,
                    _ => return Err(NativeError::Decline),
                };
                Ok(Value::Bool(match self.relation {
                    Relation::Lt => order < 0,
                    Relation::Gt => order > 0,
                    Relation::Le => order <= 0,
                    Relation::Ge => order >= 0,
                }))
            }
            _ => Err(NativeError::Decline),
        }
    }
}

/// Float operands already are ECMA Numbers. Every conversion case, including
/// guest valueOf, is declined to the complete operation body.
struct ToNumber;
impl NativeFunction for ToNumber {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match call.args.values() {
            [value @ Value::Float(_)] => Ok(*value),
            _ => Err(NativeError::Decline),
        }
    }
}

struct Subtract {
    native: Arc<dyn NativeFunction>,
}
impl NativeFunction for Subtract {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        match call.args.values() {
            // The call's two arguments are the kernel function's.
            [Value::Float(_), Value::Float(_)] => self.native.call(call),
            _ => Err(NativeError::Decline),
        }
    }
}

/// `ts.builtin_name`: whether a short text is a name some built-in row
/// answers a computed read for, by halving the generated sorted table the
/// body's search halves. A long text declines to the body.
struct BuiltinName;

fn builtin_name(args: &[Value]) -> Option<Value> {
    let [Value::Text(name)] = args else {
        return None;
    };
    // Code point order is byte order for UTF-8, as the table is sorted.
    let found = plans::BUILTIN_NAMES.binary_search(&name.as_str()).is_ok();
    Some(Value::Bool(found))
}

impl NativeFunction for BuiltinName {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        builtin_name(call.args.values()).ok_or(NativeError::Decline)
    }

    fn prepare(&self) -> Option<PreparedNative> {
        Some(PreparedNative {
            pair: None,
            entry: builtin_name,
            charged_entry: None,
            operation: "ts.builtin_name(name: Text) -> Bool",
        })
    }
}
