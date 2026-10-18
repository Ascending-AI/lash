//! The TypeScript dialect's helpers: closed ones as Rust plans compiled from
//! their kernel bodies (FIG-6678), and the hot operations that can reach
//! guest code as partial natives (FIG-6440).
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

use std::sync::Arc;

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::{
    Float, FunctionId, FunctionRegistry, NativeCall, NativeError, NativeFunction, PreparedAnswer,
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

#[rustfmt::skip]
#[path = "generated/plans.rs"]
mod plans;

pub use plans::PLANS;

/// The helpers this crate has a native for, by name.
pub const OPERATIONS: [&str; 8] = [
    "ts.get",
    "ts.add",
    "ts.sub",
    "ts.to_number",
    "ts.lt",
    "ts.le",
    "ts.gt",
    "ts.ge",
];

/// Installs each plan of [`PLANS`] beside the helper of `helpers` it was
/// compiled from, then adds its native to each of `helpers` that this
/// crate implements ([`OPERATIONS`]), each looked up by the name its
/// definition carries. Closed kernel callee plans are installed as
/// dependencies, sharing an already installed kernel plan across dialects.
/// The registry must hold the kernel library already: the natives call
/// its `num.add`, `num.sub`, `text.concat` and `text.utf16_compare`.
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
    let kernel = |name: &'static str| {
        registry
            .native_named(name)
            .ok_or(ImplementError::Missing(name))
    };
    let add = Arc::new(Add {
        add: kernel("num.add")?,
        concat: kernel("text.concat")?,
    });
    let subtract = Arc::new(Subtract {
        native: kernel("num.sub")?,
    });
    let compare = kernel("text.utf16_compare")?;
    let mut natives: Vec<(FunctionId, Arc<dyn NativeFunction>)> = Vec::new();
    for function in helpers {
        let registered = registry
            .get(&function)
            .ok_or(RegistryError::NotRegistered { function })?;
        let relation = |relation| -> Arc<dyn NativeFunction> {
            Arc::new(Relate {
                relation,
                compare: Arc::clone(&compare),
            })
        };
        let native: Arc<dyn NativeFunction> = match registered.definition().name.as_str() {
            "ts.get" => Arc::new(Get),
            "ts.add" => add.clone(),
            "ts.sub" => subtract.clone(),
            "ts.to_number" => Arc::new(ToNumber),
            "ts.lt" => relation(Relation::Lt),
            "ts.le" => relation(Relation::Le),
            "ts.gt" => relation(Relation::Gt),
            "ts.ge" => relation(Relation::Ge),
            _ => continue,
        };
        natives.push((function, native));
    }
    for (function, native) in natives {
        registry.implement(function, native)?;
    }
    Ok(())
}

fn pair<'a>(call: &'a NativeCall<'_, '_>) -> (&'a Value, &'a Value) {
    match call.args.values() {
        [left, right, ..] => (left, right),
        [left] => (left, &Value::Absent),
        [] => (&Value::Absent, &Value::Absent),
    }
}

/// `ts.add`, `a + b`, where neither operand needs ToPrimitive: two floats
/// add, two texts join, and a float and an integer add once the integer is
/// a float. Every other pair declines.
struct Add {
    add: Arc<dyn NativeFunction>,
    concat: Arc<dyn NativeFunction>,
}

impl NativeFunction for Add {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        // The call's two arguments are the kernel function's: `num.add`
        // makes the integer a float as `num.to_float` does.
        match pair(&call) {
            (Value::Float(_), Value::Float(_))
            | (Value::Float(_), Value::Int(_) | Value::BigInt(_))
            | (Value::Int(_) | Value::BigInt(_), Value::Float(_)) => self.add.call(call),
            (Value::Text(_) | Value::LongText(_), Value::Text(_) | Value::LongText(_)) => {
                self.concat.call(call)
            }
            _ => Err(NativeError::Decline),
        }
    }

    fn prepare(&self) -> Option<PreparedNative> {
        fn entry(args: &[Value]) -> Option<Value> {
            match args {
                [Value::Float(left), Value::Float(right), ..] => {
                    Some(Value::Float(Float::new(left.get() + right.get())))
                }
                _ => None,
            }
        }
        Some(PreparedNative {
            entry,
            charged_entry: Some(|args| entry(args).map(float_answer)),
            operation: "ts.add / Float",
        })
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

    fn prepare(&self) -> Option<PreparedNative> {
        let entry: fn(&[Value]) -> Option<Value> = match self.relation {
            Relation::Lt => |args| floats(args, Relation::Lt),
            Relation::Le => |args| floats(args, Relation::Le),
            Relation::Gt => |args| floats(args, Relation::Gt),
            Relation::Ge => |args| floats(args, Relation::Ge),
        };
        let charged_entry: fn(&[Value]) -> Option<PreparedAnswer> = match self.relation {
            Relation::Lt => |args| floats(args, Relation::Lt).map(float_answer),
            Relation::Le => |args| floats(args, Relation::Le).map(float_answer),
            Relation::Gt => |args| floats(args, Relation::Gt).map(float_answer),
            Relation::Ge => |args| floats(args, Relation::Ge).map(float_answer),
        };
        Some(PreparedNative {
            entry,
            charged_entry: Some(charged_entry),
            operation: "ts relation / Float",
        })
    }
}

fn floats(args: &[Value], relation: Relation) -> Option<Value> {
    match args {
        [Value::Float(left), Value::Float(right), ..] => {
            Some(Value::Bool(relation.floats(left.get(), right.get())))
        }
        _ => None,
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
    fn prepare(&self) -> Option<PreparedNative> {
        fn entry(args: &[Value]) -> Option<Value> {
            match args {
                [value @ Value::Float(_)] => Some(*value),
                _ => None,
            }
        }
        Some(PreparedNative {
            entry,
            charged_entry: Some(|args| entry(args).map(float_answer)),
            operation: "ts.to_number / Float",
        })
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
    fn prepare(&self) -> Option<PreparedNative> {
        fn entry(args: &[Value]) -> Option<Value> {
            match args {
                [Value::Float(left), Value::Float(right)] => {
                    Some(Value::Float(Float::new(left.get() - right.get())))
                }
                _ => None,
            }
        }
        Some(PreparedNative {
            entry,
            charged_entry: Some(|args| entry(args).map(float_answer)),
            operation: "ts.sub / Float",
        })
    }
}

/// P is a record receiver, text key and a brand unequal to the empty tuple.
/// Every other input declines before coercion or allocation.
struct Get;

impl NativeFunction for Get {
    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let args = call.args;
        let [
            record @ Value::Record(_),
            key @ (Value::Text(_) | Value::LongText(_)),
        ] = args.values()
        else {
            return Err(NativeError::Decline);
        };
        let Some(fields) = args.table(record)? else {
            return Err(NativeError::Malformed("a record names another body"));
        };
        let marker = ShortText::new("__ts_brand").map(Value::Text);
        let brand = match &marker {
            Some(marker) => args.lookup(fields, marker)?,
            None => None,
        };
        if let Some(brand @ Value::Tuple(_)) = brand
            && args.sequence(brand)?.is_some_and(|tuple| tuple.is_empty())
        {
            return Err(NativeError::Decline);
        }
        Ok(args.lookup(fields, key)?.copied().unwrap_or(Value::Absent))
    }
}

/// The float guards certify size-one arguments and result.
fn float_answer(value: Value) -> PreparedAnswer {
    PreparedAnswer {
        value,
        scalar_sizes: Some(0),
    }
}
