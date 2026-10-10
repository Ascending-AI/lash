//! The numeric catalogue is also the dispatch table: signatures, typed
//! errors and charges are created beside the operation that implements them.

use std::cmp::Ordering;
use std::sync::Arc;

use lash_kernel_doc::{
    ErrorValue, Float, Formula, FunctionDefinition, FunctionRegistry, Guard, Identity,
    Implementation, Integer, KERNEL_VERSION, Name, NativeCall, NativeError, NativeFunction,
    Operand, Param, QualifiedName, RegistryError, Signature, Type, Value, ValueKind,
};
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::arithmetic::{self, Binary, Unary};
use crate::comparison::{compare, equal, same};
use crate::math::Math;
use crate::numeric::{float_to_integer, integer_to_float};
use crate::raised;

#[derive(Clone, Copy, Debug)]
enum Domain {
    Any,
    Number,
    Int,
    Float,
}

impl Domain {
    fn ty(self) -> Type {
        match self {
            Self::Any => Type::Any,
            Self::Number => Type::Number,
            Self::Int => Type::Int,
            Self::Float => Type::Float,
        }
    }
    fn accepts(self, value: &Value) -> bool {
        match self {
            Self::Any => true,
            Self::Number => matches!(value, Value::Int(_) | Value::Float(_)),
            Self::Int => matches!(value, Value::Int(_)),
            Self::Float => matches!(value, Value::Float(_)),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Operation {
    Binary(Binary),
    Unary(Unary),
    Eq,
    Same,
    Lt,
    Le,
    Gt,
    Ge,
    Compare,
    Ref,
    Kind,
    ToFloat,
    ToInt,
    ToText,
    IntText,
    IntParse,
    FloatParse,
    Math(Math),
}

#[derive(Clone, Debug)]
struct NumericFunction {
    operation: Operation,
    domain: Domain,
    params: Vec<Type>,
}

/// The native numbers, comparisons, identity and math definitions.
///
/// Every operation is registered with its exact operand domain. Generic
/// equality accepts everything; numeric equality accepts only numbers, and
/// integer/float variants accept only that kind. See `docs/kernel/numbers.md`.
#[expect(
    clippy::expect_used,
    reason = "catalogue names are fixed valid identifiers, not guest data"
)]
pub fn numbers() -> Vec<(FunctionDefinition, Arc<dyn NativeFunction>)> {
    let mut functions = Vec::new();
    let mut add = |name: &str,
                   domain: Domain,
                   operation: Operation,
                   params: Vec<Type>,
                   result: Type| {
        let param_defs: Vec<Param> = params
            .iter()
            .enumerate()
            .map(|(index, ty)| Param {
                name: Name::new(format!("arg{index}")),
                ty: ty.clone(),
                optional: false,
            })
            .collect();
        let sizes: Vec<Formula> = param_defs
            .iter()
            .map(|param| Formula::DeepSize(Operand::Param(param.name.clone())))
            .collect();
        let input = Formula::Sum(sizes);
        // Structural map/record equality can inspect every pair of entries;
        // product charging bounds that work without observing hash layout.
        let work = match operation {
            Operation::Eq
            | Operation::Compare
            | Operation::Lt
            | Operation::Le
            | Operation::Gt
            | Operation::Ge
            | Operation::Binary(
                Binary::Mul
                | Binary::Div
                | Binary::DivFloor
                | Binary::DivTrunc
                | Binary::RemFloor
                | Binary::RemTrunc,
            ) => Formula::Product(vec![input.clone(), input]),
            // A float power is one libm call; only an integer power does
            // work that grows with the exponent.
            Operation::Binary(Binary::Pow) if matches!(domain, Domain::Float) => input,
            Operation::Binary(Binary::Pow) => Formula::Product(vec![
                input,
                Formula::Magnitude(Operand::Param(Name::new("arg1"))),
            ]),
            Operation::Math(_) => Formula::Sum(vec![Formula::Constant(64), input]),
            // `kind` reads a tag, and `same` compares two heap objects by
            // identity and two immutable values member by member: neither
            // walks what a heap object holds, so neither is charged for it.
            Operation::Kind => Formula::Constant(0),
            Operation::Same => Formula::Min(
                param_defs
                    .iter()
                    .map(|param| Formula::Size(Operand::Param(param.name.clone())))
                    .collect(),
            ),
            _ => input,
        };
        let charge = Formula::Sum(vec![
            Formula::Constant(1),
            work,
            Formula::DeepSize(Operand::Result),
        ]);
        let guard = (matches!(operation, Operation::Binary(Binary::Pow))
            && !matches!(domain, Domain::Float))
        .then(|| Guard {
            unit:
                "upper-bound 64-bit output magnitude words of each integer exponentiation product"
                    .into(),
            limit: Formula::Constant(1_048_576),
        });
        let mut errors = vec!["type_error".to_owned(), "arity".to_owned()];
        match operation {
            Operation::Binary(_) | Operation::ToFloat | Operation::ToInt | Operation::Math(_) => {
                errors.push("number_range".into())
            }
            Operation::Compare => errors.push("unordered".into()),
            Operation::IntParse | Operation::FloatParse => {
                errors.push("number_parse".into());
                errors.push("number_range".into());
            }
            Operation::IntText => errors.push("number_range".into()),
            _ => {}
        }
        if matches!(
            operation,
            Operation::Binary(
                Binary::Div
                    | Binary::DivFloor
                    | Binary::DivTrunc
                    | Binary::RemFloor
                    | Binary::RemTrunc
            )
        ) {
            errors.push("division_by_zero".into());
        }
        if matches!(
            (domain, operation),
            (
                Domain::Any,
                Operation::Eq | Operation::Same | Operation::Kind
            )
        ) {
            errors.retain(|kind| kind == "arity");
        }
        let definition = FunctionDefinition {
            kernel: KERNEL_VERSION,
            name: QualifiedName::new(name).expect("fixed catalogue name is valid"),
            signature: Signature {
                params: param_defs,
                result,
            },
            errors: errors.into_iter().collect(),
            charge,
            guard,
            implementation: Implementation::Native,
            native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
        };
        let native: Arc<dyn NativeFunction> = Arc::new(NumericFunction {
            operation,
            domain,
            params,
        });
        functions.push((definition, native));
    };
    for (prefix, domain) in [
        ("", Domain::Number),
        ("num.", Domain::Number),
        ("int.", Domain::Int),
        ("float.", Domain::Float),
    ] {
        for (name, op) in [
            ("add", Binary::Add),
            ("sub", Binary::Sub),
            ("mul", Binary::Mul),
            ("div", Binary::Div),
            ("div_floor", Binary::DivFloor),
            ("div_trunc", Binary::DivTrunc),
            ("rem_floor", Binary::RemFloor),
            ("rem_trunc", Binary::RemTrunc),
            ("pow", Binary::Pow),
            ("min", Binary::Min),
            ("max", Binary::Max),
        ] {
            add(
                &format!("{prefix}{name}"),
                domain,
                Operation::Binary(op),
                vec![domain.ty(); 2],
                if matches!(op, Binary::Div) {
                    Type::Float
                } else {
                    domain.ty()
                },
            );
        }
        for (name, op) in [
            ("neg", Unary::Neg),
            ("abs", Unary::Abs),
            ("floor", Unary::Floor),
            ("ceil", Unary::Ceil),
            ("trunc", Unary::Trunc),
            ("round_even", Unary::RoundEven),
            ("round_away", Unary::RoundAway),
            ("round_up", Unary::RoundUp),
            ("sign", Unary::Sign),
            ("is_finite", Unary::IsFinite),
            ("is_infinite", Unary::IsInfinite),
            ("is_nan", Unary::IsNan),
            ("is_integer", Unary::IsInteger),
        ] {
            let result = if matches!(
                op,
                Unary::IsFinite | Unary::IsInfinite | Unary::IsNan | Unary::IsInteger
            ) {
                Type::Bool
            } else {
                domain.ty()
            };
            add(
                &format!("{prefix}{name}"),
                domain,
                Operation::Unary(op),
                vec![domain.ty()],
                result,
            );
        }
    }
    for (prefix, domain) in [
        ("", Domain::Any),
        ("num.", Domain::Number),
        ("int.", Domain::Int),
        ("float.", Domain::Float),
    ] {
        for (name, op) in [
            ("eq", Operation::Eq),
            ("lt", Operation::Lt),
            ("le", Operation::Le),
            ("gt", Operation::Gt),
            ("ge", Operation::Ge),
            ("compare", Operation::Compare),
        ] {
            add(
                &format!("{prefix}{name}"),
                domain,
                op,
                vec![domain.ty(); 2],
                if matches!(op, Operation::Compare) {
                    Type::Int
                } else {
                    Type::Bool
                },
            );
        }
    }
    add(
        "kind",
        Domain::Any,
        Operation::Kind,
        vec![Type::Any],
        Type::Text,
    );
    add(
        "same",
        Domain::Any,
        Operation::Same,
        vec![Type::Any; 2],
        Type::Bool,
    );
    add(
        "ref",
        Domain::Any,
        Operation::Ref,
        vec![Type::Any],
        Type::Any,
    );
    for (name, domain, op, result) in [
        ("int.to_float", Domain::Int, Operation::ToFloat, Type::Float),
        ("float.to_int", Domain::Float, Operation::ToInt, Type::Int),
        (
            "num.to_float",
            Domain::Number,
            Operation::ToFloat,
            Type::Float,
        ),
        ("num.to_int", Domain::Number, Operation::ToInt, Type::Int),
        ("num.to_text", Domain::Number, Operation::ToText, Type::Text),
        (
            "float.to_text",
            Domain::Float,
            Operation::ToText,
            Type::Text,
        ),
    ] {
        add(name, domain, op, vec![domain.ty()], result);
    }
    add(
        "int.to_text",
        Domain::Any,
        Operation::IntText,
        vec![Type::Int, Type::Int],
        Type::Text,
    );
    add(
        "int.parse",
        Domain::Any,
        Operation::IntParse,
        vec![Type::Text, Type::Int],
        Type::Int,
    );
    add(
        "float.parse",
        Domain::Any,
        Operation::FloatParse,
        vec![Type::Text],
        Type::Float,
    );
    for (name, op) in [
        ("acos", Math::Acos),
        ("acosh", Math::Acosh),
        ("asin", Math::Asin),
        ("asinh", Math::Asinh),
        ("atan", Math::Atan),
        ("atanh", Math::Atanh),
        ("cbrt", Math::Cbrt),
        ("cos", Math::Cos),
        ("cosh", Math::Cosh),
        ("erf", Math::Erf),
        ("erfc", Math::Erfc),
        ("exp", Math::Exp),
        ("exp2", Math::Exp2),
        ("expm1", Math::Expm1),
        ("gamma", Math::Gamma),
        ("lgamma", Math::Lgamma),
        ("log", Math::Log),
        ("log2", Math::Log2),
        ("log10", Math::Log10),
        ("log1p", Math::Log1p),
        ("sin", Math::Sin),
        ("sinh", Math::Sinh),
        ("sqrt", Math::Sqrt),
        ("tan", Math::Tan),
        ("tanh", Math::Tanh),
        ("atan2", Math::Atan2),
        ("hypot", Math::Hypot),
        ("copysign", Math::CopySign),
        ("nextafter", Math::NextAfter),
        ("remainder", Math::Remainder),
        ("pow", Math::Pow),
        ("fma", Math::Fma),
    ] {
        add(
            &format!("math.{name}"),
            Domain::Number,
            Operation::Math(op),
            vec![Type::Number; op.arity()],
            Type::Float,
        );
    }
    functions
}

/// Registers the numeric catalogue. No runtime or global registry is hidden.
pub fn register_numbers(registry: &mut FunctionRegistry) -> Result<(), RegistryError> {
    for (definition, native) in numbers() {
        registry.register(definition, Some(native))?;
    }
    Ok(())
}

impl NativeFunction for NumericFunction {
    fn call(&self, call: NativeCall<'_>) -> Result<Value, NativeError> {
        if call.args.len() != self.params.len() {
            return Err(raised("arity", "wrong number of arguments"));
        }
        for (arg, ty) in call.args.iter().zip(&self.params) {
            let accepted = match ty {
                Type::Any => true,
                Type::Int => matches!(arg, Value::Int(_)),
                Type::Float => matches!(arg, Value::Float(_)),
                Type::Number => matches!(arg, Value::Int(_) | Value::Float(_)),
                Type::Text => matches!(arg, Value::Text(_)),
                _ => false,
            };
            if !accepted {
                return Err(raised("type_error", "argument has the wrong kind"));
            }
        }
        if !matches!(
            self.operation,
            Operation::IntText | Operation::IntParse | Operation::FloatParse
        ) && !call.args.iter().all(|arg| self.domain.accepts(arg))
        {
            return Err(raised(
                "type_error",
                "argument is outside the function's domain",
            ));
        }
        let a = &call.args[0];
        match self.operation {
            Operation::Binary(op) => {
                arithmetic::binary(op, a, &call.args[1], call.counter, call.heap)
            }
            Operation::Unary(op) => arithmetic::unary(op, a),
            Operation::Math(op) => op.call(call.args),
            Operation::Eq => Ok(Value::Bool(equal(a, &call.args[1], call.heap))),
            Operation::Same => Ok(Value::Bool(same(a, &call.args[1]))),
            Operation::Lt | Operation::Le | Operation::Gt | Operation::Ge | Operation::Compare => {
                let order = compare(a, &call.args[1], call.heap)?;
                Ok(match self.operation {
                    Operation::Compare => Value::Int(Integer::from(match order {
                        Some(Ordering::Less) => -1,
                        Some(Ordering::Equal) => 0,
                        Some(Ordering::Greater) => 1,
                        None => return Err(raised("unordered", "NaN has no ordering")),
                    })),
                    Operation::Lt => Value::Bool(order == Some(Ordering::Less)),
                    Operation::Le => {
                        Value::Bool(matches!(order, Some(Ordering::Less | Ordering::Equal)))
                    }
                    Operation::Gt => Value::Bool(order == Some(Ordering::Greater)),
                    _ => Value::Bool(matches!(order, Some(Ordering::Greater | Ordering::Equal))),
                })
            }
            Operation::Kind => Ok(Value::text(match a.kind() {
                ValueKind::Null => "null",
                ValueKind::Absent => "absent",
                ValueKind::Bool => "bool",
                ValueKind::Int => "integer",
                ValueKind::Float => "float",
                ValueKind::Text => "text",
                ValueKind::Bytes => "bytes",
                ValueKind::Timestamp => "timestamp",
                ValueKind::Tuple => "tuple",
                ValueKind::List => "list",
                ValueKind::Map => "map",
                ValueKind::Set => "set",
                ValueKind::Record => "record",
                ValueKind::Closure => "closure",
                ValueKind::Error => "error",
                ValueKind::Task => "task",
                ValueKind::Function => "function",
                ValueKind::Handle => "handle",
                ValueKind::Ref => "ref",
            })),
            Operation::Ref => {
                let identity =
                    match a {
                        Value::Task(id) => Identity::Task(*id),
                        _ => Identity::Object(a.object().ok_or_else(|| {
                            raised("type_error", "ref requires an object or task")
                        })?),
                    };
                Ok(Value::Ref(identity))
            }
            Operation::ToFloat => match a {
                Value::Int(value) => integer_to_float(value).map(Value::Float),
                Value::Float(_) => Ok(a.clone()),
                _ => Err(raised("type_error", "expected a number")),
            },
            Operation::ToInt => match a {
                Value::Float(value) => float_to_integer(*value).map(Value::Int),
                Value::Int(_) => Ok(a.clone()),
                _ => Err(raised("type_error", "expected a number")),
            },
            Operation::ToText => match a {
                Value::Int(value) => Ok(Value::text(value.to_string())),
                Value::Float(value) => Ok(Value::text(value.to_string())),
                _ => Err(raised("type_error", "expected a number")),
            },
            Operation::IntText => {
                let radix = radix(&call.args[1])?;
                if let Value::Int(value) = a {
                    Ok(Value::text(value.as_bigint().to_str_radix(radix)))
                } else {
                    Err(raised("type_error", "expected an integer"))
                }
            }
            Operation::IntParse => {
                let radix = radix(&call.args[1])?;
                if let Value::Text(text) = a {
                    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
                    if digits.is_empty()
                        || !digits.bytes().all(|byte| {
                            byte.is_ascii_alphanumeric() && (byte as char).is_digit(radix)
                        })
                    {
                        return Err(raised(
                            "number_parse",
                            "expected signed radix digits without whitespace or separators",
                        ));
                    }
                    BigInt::parse_bytes(text.as_bytes(), radix)
                        .map(|value| Value::Int(Integer::new(value)))
                        .ok_or_else(|| raised("number_parse", "invalid integer text"))
                } else {
                    Err(raised("type_error", "expected text"))
                }
            }
            Operation::FloatParse => {
                if let Value::Text(text) = a {
                    parse_float(text).map(|value| Value::Float(Float::new(value)))
                } else {
                    Err(raised("type_error", "expected text"))
                }
            }
        }
    }
}

fn radix(value: &Value) -> Result<u32, NativeError> {
    match value {
        Value::Int(value) => value
            .to_u32()
            .filter(|value| (2..=36).contains(value))
            .ok_or_else(|| raised("number_range", "radix must be between 2 and 36")),
        _ => Err(raised("type_error", "radix must be an integer")),
    }
}

fn parse_float(text: &str) -> Result<f64, NativeError> {
    match text {
        "nan" => return Ok(f64::NAN),
        "inf" | "+inf" => return Ok(f64::INFINITY),
        "-inf" => return Ok(f64::NEG_INFINITY),
        _ => {}
    }
    let invalid = || {
        raised(
            "number_parse",
            "expected decimal digits with optional sign, point and exponent",
        )
    };
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(text);
    let parts: Vec<&str> = unsigned.split(['e', 'E']).collect();
    if parts.len() > 2 {
        return Err(invalid());
    }
    if parts.len() == 2 {
        let exponent = parts[1].strip_prefix(['+', '-']).unwrap_or(parts[1]);
        if exponent.is_empty() || !exponent.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
    }
    let mut digits = 0;
    let mut points = 0;
    for byte in parts[0].bytes() {
        if byte.is_ascii_digit() {
            digits += 1;
        } else if byte == b'.' {
            points += 1;
        } else {
            return Err(invalid());
        }
    }
    if digits == 0 || points > 1 {
        return Err(invalid());
    }
    let value: f64 = text.parse().map_err(|_| invalid())?;
    if value.is_infinite() {
        return Err(NativeError::Raised(ErrorValue::new(
            "number_range",
            "decimal text rounds to infinity",
        )));
    }
    Ok(value)
}
