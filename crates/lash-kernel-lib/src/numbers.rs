//! The numeric catalogue is also the dispatch table: signatures, typed
//! errors and charges are created beside the operation that implements them.

use std::cmp::Ordering;
use std::sync::{Arc, LazyLock};

use lash_kernel_doc::{
    Float, Formula, FunctionDefinition, FunctionRegistry, Guard, Identity, Implementation, Integer,
    KERNEL_VERSION, Name, NativeCall, NativeError, NativeFunction, NativeHeap, Operand, Param,
    PreparedNative, QualifiedName, RegistryError, Signature, Type, Value, ValueKind,
};
use num_bigint::BigInt;
use num_traits::ToPrimitive;

use crate::arithmetic::{self, Binary, Unary};
use crate::comparison::{compare, equal, same};
use crate::math::Math;
use crate::numeric::{Numeric, float_to_integer, integer_to_float};
use lash_kernel_doc::heap::{IntRef, Payload};

mod prepared;
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
            // `kind` reads a tag. `same` compares two heap objects by identity
            // and two immutable values member by member: it walks no heap
            // object's contents, but walks every member nested in a tuple or
            // an error's data. Each operand's nested size bounds that walk,
            // and is what measuring it costs.
            Operation::Kind => Formula::Constant(0),
            Operation::Same => Formula::Sum(
                std::iter::once(Formula::Min(
                    param_defs
                        .iter()
                        .map(|param| Formula::Size(Operand::Param(param.name.clone())))
                        .collect(),
                ))
                .chain(
                    param_defs
                        .iter()
                        .map(|param| Formula::NestedSize(Operand::Param(param.name.clone()))),
                )
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
        self.admit(call.args)?;
        if let Operation::Binary(op) = self.operation {
            return arithmetic::binary(op, &call.args[0], &call.args[1], call.counter, call.heap);
        }
        if let Some(result) = plain(self.operation, call.args, Some(&mut *call.heap))? {
            return Ok(result);
        }
        let a = &call.args[0];
        match self.operation {
            Operation::Binary(op) => {
                arithmetic::binary(op, a, &call.args[1], call.counter, call.heap)
            }
            Operation::ToText => match a {
                Value::Int(value) => integer_text(call.heap, value, 10),
                _ => unreachable!("only an integer's digits are reserved"),
            },
            Operation::IntText => {
                let radix = radix(&call.args[1])?;
                if let Value::Int(value) = a {
                    integer_text(call.heap, value, radix)
                } else {
                    Err(raised("type_error", "expected an integer"))
                }
            }
            _ => unreachable!("only an integer power counts its work or digits reserve room"),
        }
    }

    fn prepare(&self) -> Option<PreparedNative> {
        prepared::entry(self.operation, self.domain)
    }
}

impl NumericFunction {
    /// What every call checks first: the arity, each argument's kind and
    /// the operation's domain.
    fn admit(&self, args: &[Value]) -> Result<(), NativeError> {
        if args.len() != self.params.len() {
            return Err(raised("arity", "wrong number of arguments"));
        }
        for (arg, ty) in args.iter().zip(&self.params) {
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
        ) && !args.iter().all(|arg| self.domain.accepts(arg))
        {
            return Err(raised(
                "type_error",
                "argument is outside the function's domain",
            ));
        }
        Ok(())
    }
}

/// The one implementation of admitted scalar operations. A heap-dependent
/// comparison leaves a prepared entry for the general call (K-LIB-006).
#[inline]
fn plain(
    operation: Operation,
    args: &[Value],
    mut heap: Option<&mut dyn NativeHeap>,
) -> Result<Option<Value>, NativeError> {
    let a = &args[0];
    Ok(Some(match operation {
        Operation::Binary(op) => {
            let Some(left) = Numeric::resident(a) else {
                return Ok(None);
            };
            let Some(right) = Numeric::resident(&args[1]) else {
                return Ok(None);
            };
            return Ok(arithmetic::plain(op, &left, &right)?.and_then(Numeric::inline));
        }
        Operation::Unary(op) => match heap.as_deref_mut() {
            Some(heap) => arithmetic::unary(op, a, heap)?,
            None => {
                let Some(value) = Numeric::resident(a) else {
                    return Ok(None);
                };
                let Some(result) = arithmetic::scalar_unary(op, &value)?.inline() else {
                    return Ok(None);
                };
                result
            }
        },
        Operation::Math(op) => {
            let numbers = match heap.as_deref_mut() {
                Some(heap) => args
                    .iter()
                    .map(|v| Numeric::read(v, heap))
                    .collect::<Result<Vec<_>, _>>()?,
                None => {
                    let Some(numbers) = args
                        .iter()
                        .map(Numeric::resident)
                        .collect::<Option<Vec<_>>>()
                    else {
                        return Ok(None);
                    };
                    numbers
                }
            };
            op.call(&numbers)?.inline().expect("math returns a float")
        }
        Operation::Eq => {
            let equal = match heap {
                Some(heap) => equal(a, &args[1], heap)?,
                None => match crate::comparison::scalar_equal(a, &args[1]) {
                    Some(equal) => equal,
                    None => return Ok(None),
                },
            };
            Value::Bool(equal)
        }
        Operation::Same => Value::Bool(match heap.as_deref_mut() {
            Some(heap) => same(a, &args[1], heap)?,
            None => {
                let Some(same) = crate::comparison::scalar_same(a, &args[1]) else {
                    return Ok(None);
                };
                same
            }
        }),
        Operation::Lt | Operation::Le | Operation::Gt | Operation::Ge | Operation::Compare => {
            let order = match heap {
                Some(heap) => compare(a, &args[1], heap)?,
                None => match crate::comparison::scalar_compare(a, &args[1]) {
                    Some(order) => order?,
                    None => return Ok(None),
                },
            };
            match operation {
                Operation::Compare => Value::Int(IntRef::Inline(Integer::from(match order {
                    Some(Ordering::Less) => -1,
                    Some(Ordering::Equal) => 0,
                    Some(Ordering::Greater) => 1,
                    None => return Err(raised("unordered", "NaN has no ordering")),
                }))),
                Operation::Lt => Value::Bool(order == Some(Ordering::Less)),
                Operation::Le => {
                    Value::Bool(matches!(order, Some(Ordering::Less | Ordering::Equal)))
                }
                Operation::Gt => Value::Bool(order == Some(Ordering::Greater)),
                _ => Value::Bool(matches!(order, Some(Ordering::Greater | Ordering::Equal))),
            }
        }
        Operation::Kind => kind_name(a.kind()),
        Operation::Ref => Value::Ref(match a {
            Value::Task(id) => Identity::Task(*id),
            _ => Identity::Object(
                a.object()
                    .ok_or_else(|| raised("type_error", "ref requires an object or task"))?,
            ),
        }),
        Operation::ToFloat => match a {
            Value::Int(value) => {
                let integer = match value {
                    IntRef::Inline(i) => i.clone(),
                    _ => {
                        let Some(heap) = heap.as_deref_mut() else {
                            return Ok(None);
                        };
                        heap.materialize_int(value)?
                    }
                };
                Value::Float(integer_to_float(&integer)?)
            }
            Value::Float(_) => a.clone(),
            _ => return Err(raised("type_error", "expected a number")),
        },
        Operation::ToInt => match a {
            Value::Float(value) => Value::Int(IntRef::Inline(float_to_integer(*value)?)),
            Value::Int(_) => a.clone(),
            _ => return Err(raised("type_error", "expected a number")),
        },
        Operation::ToText => match a {
            // Its digits are reserved before they are written.
            Value::Int(_) => return Ok(None),
            Value::Float(value) => Value::text(value.to_string()),
            _ => return Err(raised("type_error", "expected a number")),
        },
        Operation::IntText => {
            // Its digits are reserved before they are written.
            return Ok(None);
        }
        Operation::IntParse => {
            let radix = radix(&args[1])?;
            let Value::Text(text) = a else {
                return Err(raised("type_error", "expected text"));
            };
            let text = match &text.body {
                Payload::Resident(text) => Arc::clone(text),
                _ => {
                    let Some(heap) = heap.as_deref_mut() else {
                        return Ok(None);
                    };
                    heap.materialize_text(text)?
                }
            };
            let digits = text.strip_prefix(['+', '-']).unwrap_or(&text);
            if digits.is_empty()
                || !digits
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() && (byte as char).is_digit(radix))
            {
                return Err(raised(
                    "number_parse",
                    "expected signed radix digits without whitespace or separators",
                ));
            }
            let integer = Integer::new(
                BigInt::parse_bytes(text.as_bytes(), radix)
                    .ok_or_else(|| raised("number_parse", "invalid integer text"))?,
            );
            match heap.as_deref_mut() {
                Some(heap) => Value::Int(heap.int(integer)?),
                None => {
                    let Some(value) = Numeric::Int(integer).inline() else {
                        return Ok(None);
                    };
                    value
                }
            }
        }
        Operation::FloatParse => {
            let Value::Text(text) = a else {
                return Err(raised("type_error", "expected text"));
            };
            let text = match &text.body {
                Payload::Resident(text) => Arc::clone(text),
                _ => {
                    let Some(heap) = heap.as_deref_mut() else {
                        return Ok(None);
                    };
                    heap.materialize_text(text)?
                }
            };
            Value::Float(Float::new(parse_float(&text)?))
        }
    }))
}

/// Every kind with the name `kind` answers for it, in the order of
/// [`ValueKind`]'s variants.
pub(crate) const KIND_NAMES: [(ValueKind, &str); 19] = [
    (ValueKind::Null, "null"),
    (ValueKind::Absent, "absent"),
    (ValueKind::Bool, "bool"),
    (ValueKind::Int, "integer"),
    (ValueKind::Float, "float"),
    (ValueKind::Text, "text"),
    (ValueKind::Bytes, "bytes"),
    (ValueKind::Timestamp, "timestamp"),
    (ValueKind::Tuple, "tuple"),
    (ValueKind::List, "list"),
    (ValueKind::Map, "map"),
    (ValueKind::Set, "set"),
    (ValueKind::Record, "record"),
    (ValueKind::Closure, "closure"),
    (ValueKind::Error, "error"),
    (ValueKind::Task, "task"),
    (ValueKind::Function, "function"),
    (ValueKind::Handle, "handle"),
    (ValueKind::Ref, "ref"),
];

const _: () = {
    let mut index = 0;
    while index < KIND_NAMES.len() {
        assert!(KIND_NAMES[index].0 as usize == index);
        index += 1;
    }
};

/// A kind's name, as `kind` answers it. Each name is made once and shared
/// by every call that answers it.
fn kind_name(kind: ValueKind) -> Value {
    static NAMES: LazyLock<[Arc<str>; 19]> =
        LazyLock::new(|| KIND_NAMES.map(|(_, name)| Arc::from(name)));
    Value::text(Arc::clone(&NAMES[kind as usize]))
}

/// An integer's digits in `radix`, a text that can be eight times the
/// integer's own bytes (radix 2). The digits are reserved, as the text and
/// as the buffer they are written in, before they are written; a refusal
/// is [`NativeError::Memory`].
pub(crate) fn integer_text(
    heap: &mut dyn NativeHeap,
    integer: &IntRef,
    radix: u32,
) -> Result<Value, NativeError> {
    // An integer below 2^bits has at most bits / log2(radix) + 1 digits;
    // one more digit absorbs the float's rounding, and one is the sign.
    let bits = match integer {
        IntRef::Inline(i) => i.bits(),
        IntRef::Paged { summary, .. } => summary.bits,
    };
    let digits = (bits as f64 / f64::from(radix).log2()) as u64 + 3;
    heap.reserve(0, digits.saturating_mul(2))?;
    let integer = match integer {
        IntRef::Inline(i) => i.clone(),
        _ => heap.materialize_int(integer)?,
    };
    Ok(Value::Text(
        heap.text(&integer.as_bigint().to_str_radix(radix))?,
    ))
}

fn radix(value: &Value) -> Result<u32, NativeError> {
    match value {
        Value::Int(IntRef::Inline(value)) => value
            .to_u32()
            .filter(|value| (2..=36).contains(value))
            .ok_or_else(|| raised("number_range", "radix must be between 2 and 36")),
        Value::Int(_) => Err(raised("number_range", "radix must be between 2 and 36")),
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
    let unsigned = text.strip_prefix(['+', '-']).unwrap_or(&text);
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
        return Err(NativeError::Raised(Arc::new(
            lash_kernel_doc::heap::ErrorRef::new("number_range", "decimal text rounds to infinity"),
        )));
    }
    Ok(value)
}
