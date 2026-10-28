//! The numeric catalogue is also the dispatch table: signatures, typed
//! errors and charges are created beside the operation that implements them.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::sync::Arc;

use lash_kernel_doc::arena::ShortText;
use lash_kernel_doc::{
    Args, Float, Formula, FunctionDefinition, FunctionRegistry, Guard, Implementation, Integer,
    KERNEL_VERSION, NativeCall, NativeError, NativeFunction, Nursery, Operand, PreparedNative,
    RegistryError, Value, ValueKind,
};

use num_bigint::BigInt;
use {
    lash_kernel_value::FunctionName, lash_kernel_value::Name, lash_kernel_value::Param,
    lash_kernel_value::Signature, lash_kernel_value::Type,
};

use crate::arithmetic::{self, Binary, Unary};
use crate::comparison::{compare, equal, same};
use crate::math::Math;
use crate::numeric::{Numeric, float_to_integer, integer_to_float};

mod prepared;
mod resources;
pub(super) const INTEGER_POWER_WORK_LIMIT: u64 = 1_048_576;
use crate::KernelHelper;
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
            Self::Number => number(value),
            Self::Int => integral(value),
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
pub fn numbers() -> Vec<(FunctionDefinition, Arc<dyn NativeFunction>)> {
    let mut functions = Vec::new();
    catalogue(None, |name, domain, operation, params, result| {
        let definition = definition(name, domain, operation, &params, result);
        let native: Arc<dyn NativeFunction> = Arc::new(NumericFunction {
            operation,
            domain,
            params,
        });
        functions.push((definition, native));
    });
    functions
}

/// Adds each numeric native to the registered definition of its name: the
/// definitions come from the build's release image (FIG-6843), so a process
/// builds the natives alone.
///
/// # Errors
///
/// The first [`FunctionRegistry::implement_named`] refuses.
pub fn implement_numbers(registry: &mut FunctionRegistry) -> Result<(), RegistryError> {
    let mut implemented = Ok(());
    catalogue(None, |name, domain, operation, params, _| {
        if implemented.is_ok() {
            implemented = registry
                .implement_named(
                    name.name(),
                    Arc::new(NumericFunction {
                        operation,
                        domain,
                        params,
                    }),
                )
                .map(|_| ());
        }
    });
    implemented
}

/// Constructs only the selected native; definition authoring uses the same catalogue.
pub(crate) fn native(selected: &str) -> Option<Arc<dyn NativeFunction>> {
    let mut found = None;
    catalogue(Some(selected), |name, domain, operation, params, _| {
        if name.name() == selected {
            found = Some(Arc::new(NumericFunction {
                operation,
                domain,
                params,
            }) as Arc<dyn NativeFunction>);
        }
    });
    found
}

/// Hands `add` each numeric function: its name, operand domain, operation,
/// parameter types and result type.
#[expect(
    clippy::expect_used,
    reason = "catalogue names are fixed valid identifiers, not guest data"
)]
fn catalogue(
    selected: Option<&str>,
    mut add: impl FnMut(KernelHelper, Domain, Operation, Vec<Type>, Type),
) {
    let mut emit =
        |name: KernelHelper, domain, operation, params: &dyn Fn() -> Vec<Type>, result| {
            if selected.is_none_or(|selected| selected == name.name()) {
                add(name, domain, operation, params(), result);
            }
        };
    for (prefix, domain) in [
        ("", Domain::Number),
        ("num.", Domain::Number),
        ("int.", Domain::Int),
        ("float.", Domain::Float),
    ] {
        for (name, op) in [
            (KernelHelper::H_add, Binary::Add),
            (KernelHelper::H_sub, Binary::Sub),
            (KernelHelper::H_mul, Binary::Mul),
            (KernelHelper::H_div, Binary::Div),
            (KernelHelper::H_div_ufloor, Binary::DivFloor),
            (KernelHelper::H_div_utrunc, Binary::DivTrunc),
            (KernelHelper::H_rem_ufloor, Binary::RemFloor),
            (KernelHelper::H_rem_utrunc, Binary::RemTrunc),
            (KernelHelper::H_pow, Binary::Pow),
            (KernelHelper::H_min, Binary::Min),
            (KernelHelper::H_max, Binary::Max),
        ] {
            if selected.is_some_and(|selected| selected.strip_prefix(prefix) != Some(name.name())) {
                continue;
            }
            emit(
                KernelHelper::from_name(&format!("{prefix}{}", name.name()))
                    .expect("declared numeric family"),
                domain,
                Operation::Binary(op),
                &|| vec![domain.ty(); 2],
                if matches!(op, Binary::Div) {
                    Type::Float
                } else {
                    domain.ty()
                },
            );
        }
        for (name, op) in [
            (KernelHelper::H_neg, Unary::Neg),
            (KernelHelper::H_abs, Unary::Abs),
            (KernelHelper::H_floor, Unary::Floor),
            (KernelHelper::H_ceil, Unary::Ceil),
            (KernelHelper::H_trunc, Unary::Trunc),
            (KernelHelper::H_round_ueven, Unary::RoundEven),
            (KernelHelper::H_round_uaway, Unary::RoundAway),
            (KernelHelper::H_round_uup, Unary::RoundUp),
            (KernelHelper::H_sign, Unary::Sign),
            (KernelHelper::H_is_ufinite, Unary::IsFinite),
            (KernelHelper::H_is_uinfinite, Unary::IsInfinite),
            (KernelHelper::H_is_unan, Unary::IsNan),
            (KernelHelper::H_is_uinteger, Unary::IsInteger),
        ] {
            let result = if matches!(
                op,
                Unary::IsFinite | Unary::IsInfinite | Unary::IsNan | Unary::IsInteger
            ) {
                Type::Bool
            } else {
                domain.ty()
            };
            if selected.is_some_and(|selected| selected.strip_prefix(prefix) != Some(name.name())) {
                continue;
            }
            emit(
                KernelHelper::from_name(&format!("{prefix}{}", name.name()))
                    .expect("declared numeric family"),
                domain,
                Operation::Unary(op),
                &|| vec![domain.ty()],
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
            (KernelHelper::H_eq, Operation::Eq),
            (KernelHelper::H_lt, Operation::Lt),
            (KernelHelper::H_le, Operation::Le),
            (KernelHelper::H_gt, Operation::Gt),
            (KernelHelper::H_ge, Operation::Ge),
            (KernelHelper::H_compare, Operation::Compare),
        ] {
            if selected.is_some_and(|selected| selected.strip_prefix(prefix) != Some(name.name())) {
                continue;
            }
            emit(
                KernelHelper::from_name(&format!("{prefix}{}", name.name()))
                    .expect("declared numeric family"),
                domain,
                op,
                &|| vec![domain.ty(); 2],
                if matches!(op, Operation::Compare) {
                    Type::Int
                } else {
                    Type::Bool
                },
            );
        }
    }
    emit(
        KernelHelper::H_kind,
        Domain::Any,
        Operation::Kind,
        &|| vec![Type::Any],
        Type::Text,
    );
    emit(
        KernelHelper::H_same,
        Domain::Any,
        Operation::Same,
        &|| vec![Type::Any; 2],
        Type::Bool,
    );
    emit(
        KernelHelper::H_ref,
        Domain::Any,
        Operation::Ref,
        &|| vec![Type::Any],
        Type::Any,
    );
    for (name, domain, op, result) in [
        (
            KernelHelper::H_int_dto_ufloat,
            Domain::Int,
            Operation::ToFloat,
            Type::Float,
        ),
        (
            KernelHelper::H_float_dto_uint,
            Domain::Float,
            Operation::ToInt,
            Type::Int,
        ),
        (
            KernelHelper::H_num_dto_ufloat,
            Domain::Number,
            Operation::ToFloat,
            Type::Float,
        ),
        (
            KernelHelper::H_num_dto_uint,
            Domain::Number,
            Operation::ToInt,
            Type::Int,
        ),
        (
            KernelHelper::H_num_dto_utext,
            Domain::Number,
            Operation::ToText,
            Type::Text,
        ),
        (
            KernelHelper::H_float_dto_utext,
            Domain::Float,
            Operation::ToText,
            Type::Text,
        ),
    ] {
        emit(name, domain, op, &|| vec![domain.ty()], result);
    }
    emit(
        KernelHelper::H_int_dto_utext,
        Domain::Any,
        Operation::IntText,
        &|| vec![Type::Int, Type::Int],
        Type::Text,
    );
    emit(
        KernelHelper::H_int_dparse,
        Domain::Any,
        Operation::IntParse,
        &|| vec![Type::Text, Type::Int],
        Type::Int,
    );
    emit(
        KernelHelper::H_float_dparse,
        Domain::Any,
        Operation::FloatParse,
        &|| vec![Type::Text],
        Type::Float,
    );
    for (name, op) in [
        (KernelHelper::H_math_dacos, Math::Acos),
        (KernelHelper::H_math_dacosh, Math::Acosh),
        (KernelHelper::H_math_dasin, Math::Asin),
        (KernelHelper::H_math_dasinh, Math::Asinh),
        (KernelHelper::H_math_datan, Math::Atan),
        (KernelHelper::H_math_datanh, Math::Atanh),
        (KernelHelper::H_math_dcbrt, Math::Cbrt),
        (KernelHelper::H_math_dcos, Math::Cos),
        (KernelHelper::H_math_dcosh, Math::Cosh),
        (KernelHelper::H_math_derf, Math::Erf),
        (KernelHelper::H_math_derfc, Math::Erfc),
        (KernelHelper::H_math_dexp, Math::Exp),
        (KernelHelper::H_math_dexp2, Math::Exp2),
        (KernelHelper::H_math_dexpm1, Math::Expm1),
        (KernelHelper::H_math_dgamma, Math::Gamma),
        (KernelHelper::H_math_dlgamma, Math::Lgamma),
        (KernelHelper::H_math_dlog, Math::Log),
        (KernelHelper::H_math_dlog2, Math::Log2),
        (KernelHelper::H_math_dlog10, Math::Log10),
        (KernelHelper::H_math_dlog1p, Math::Log1p),
        (KernelHelper::H_math_dsin, Math::Sin),
        (KernelHelper::H_math_dsinh, Math::Sinh),
        (KernelHelper::H_math_dsqrt, Math::Sqrt),
        (KernelHelper::H_math_dtan, Math::Tan),
        (KernelHelper::H_math_dtanh, Math::Tanh),
        (KernelHelper::H_math_datan2, Math::Atan2),
        (KernelHelper::H_math_dhypot, Math::Hypot),
        (KernelHelper::H_math_dcopysign, Math::CopySign),
        (KernelHelper::H_math_dnextafter, Math::NextAfter),
        (KernelHelper::H_math_dremainder, Math::Remainder),
        (KernelHelper::H_math_dpow, Math::Pow),
        (KernelHelper::H_math_dfma, Math::Fma),
    ] {
        emit(
            name,
            Domain::Number,
            Operation::Math(op),
            &|| vec![Type::Number; op.arity()],
            Type::Float,
        );
    }
}

/// The definition of numeric function `name`.
#[expect(
    clippy::expect_used,
    reason = "catalogue names are fixed valid identifiers, not guest data"
)]
fn definition(
    name: KernelHelper,
    domain: Domain,
    operation: Operation,
    params: &[Type],
    result: Type,
) -> FunctionDefinition {
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
        unit: "upper-bound 64-bit output magnitude words of each integer exponentiation product"
            .into(),
        limit: Formula::Constant(INTEGER_POWER_WORK_LIMIT),
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
            Binary::Div | Binary::DivFloor | Binary::DivTrunc | Binary::RemFloor | Binary::RemTrunc
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
    FunctionDefinition {
        projection: Default::default(),
        kernel: KERNEL_VERSION,
        name: FunctionName::new(name.name()).expect("fixed catalogue name is valid"),
        signature: Signature {
            params: param_defs,
            result,
        },
        errors: errors.into_iter().collect(),
        guard,
        implementation: Implementation::Native { charge },
        native_version: lash_kernel_doc::FIRST_NATIVE_VERSION,
    }
}

/// Registers the numeric catalogue. No runtime or global registry is hidden.
pub fn register_numbers(registry: &mut FunctionRegistry) -> Result<(), RegistryError> {
    for (definition, native) in numbers() {
        registry.register(definition, Some(native))?;
    }
    Ok(())
}

impl NativeFunction for NumericFunction {
    fn resource_contract(
        &self,
        args: Args<'_>,
    ) -> Result<Option<lash_kernel_doc::NativeResourceContract>, NativeError> {
        resources::contract(self, args)
    }
    fn specialization(&self) -> lash_kernel_doc::NativeSpecialization {
        use lash_kernel_doc::SymbolicNative;
        lash_kernel_doc::NativeSpecialization {
            immediate: self.prepare(),
            symbolic: match (self.operation, self.domain) {
                (Operation::Kind, Domain::Any) => Some(SymbolicNative::Kind),
                (Operation::Same, Domain::Any) => Some(SymbolicNative::Same),
                (Operation::ToFloat, Domain::Any | Domain::Number) => Some(SymbolicNative::ToFloat),
                (Operation::Unary(Unary::Floor), Domain::Any | Domain::Number) => {
                    Some(SymbolicNative::Floor)
                }
                (
                    Operation::Eq
                    | Operation::Lt
                    | Operation::Le
                    | Operation::Gt
                    | Operation::Ge
                    | Operation::Binary(Binary::Add),
                    Domain::Any | Domain::Number,
                ) => self
                    .prepare()
                    .and_then(|entry| entry.pair)
                    .and_then(|pair| pair.floats)
                    .map(SymbolicNative::Numeric),
                _ => None,
            },
            ..Default::default()
        }
    }

    fn call(&self, call: NativeCall<'_, '_>) -> Result<Value, NativeError> {
        let values = call.args.values();
        self.admit(values)?;
        let args = &call.args;
        if let Operation::Binary(op) = self.operation {
            return arithmetic::binary(op, &values[0], &values[1], call.counter, args, call.new);
        }
        if let Some(result) = plain(self.operation, values, Some((args, &mut *call.new)))? {
            return Ok(result);
        }
        let a = &values[0];
        match self.operation {
            Operation::ToText => integer_text(args, call.new, a, 10),
            Operation::IntText => integer_text(args, call.new, a, radix(&values[1])?),
            _ => unreachable!("only an integer's digits reserve room"),
        }
    }

    fn prepare(&self) -> Option<PreparedNative> {
        prepared::entry(self.operation, self.domain)
    }
}

fn integral(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::BigInt(_))
}

fn number(value: &Value) -> bool {
    matches!(value, Value::Int(_) | Value::BigInt(_) | Value::Float(_))
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
                Type::Int => integral(arg),
                Type::Float => matches!(arg, Value::Float(_)),
                Type::Number => number(arg),
                Type::Text => matches!(arg, Value::Text(_) | Value::LongText(_)),
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

/// A call's arena: its views and its nursery. A prepared entry has none.
type Arena<'a, 'call> = Option<(&'a Args<'call>, &'a mut Nursery<'call>)>;

/// The one implementation of admitted scalar operations. An
/// arena-dependent operation leaves a prepared entry for the general call
/// (K-LIB-006).
#[inline]
fn plain(
    operation: Operation,
    args: &[Value],
    mut arena: Arena<'_, '_>,
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
        Operation::Unary(op) => match arena {
            Some((views, new)) => arithmetic::unary(op, a, views, new)?,
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
            let numbers = match &mut arena {
                Some((views, new)) => args
                    .iter()
                    .map(|v| Numeric::read(v, views, new))
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
            {
                let Some(value) = op.call(&numbers)?.inline() else {
                    unreachable!("math returns a float")
                };
                value
            }
        }
        Operation::Eq => {
            let equal = match &arena {
                Some((views, _)) => equal(a, &args[1], views)?,
                None => match crate::comparison::scalar_equal(a, &args[1]) {
                    Some(equal) => equal,
                    None => return Ok(None),
                },
            };
            Value::Bool(equal)
        }
        Operation::Same => Value::Bool(match &arena {
            Some((views, _)) => same(a, &args[1], views)?,
            None => {
                let Some(same) = crate::comparison::scalar_same(a, &args[1]) else {
                    return Ok(None);
                };
                same
            }
        }),
        Operation::Lt | Operation::Le | Operation::Gt | Operation::Ge | Operation::Compare => {
            let order = match &mut arena {
                Some((views, new)) => compare(a, &args[1], views, new)?,
                None => match crate::comparison::scalar_compare(a, &args[1]) {
                    Some(order) => order?,
                    None => return Ok(None),
                },
            };
            relation(operation, order)?
        }

        Operation::Kind => kind_name(a.kind()),
        Operation::Ref => match a {
            Value::Task(id) => Value::TaskRef(*id),
            Value::List(object)
            | Value::Map(object)
            | Value::Set(object)
            | Value::Record(object)
            | Value::Closure(object) => Value::ObjectRef(*object),
            _ => return Err(raised("type_error", "ref requires an object or task")),
        },
        Operation::ToFloat => match a {
            Value::Int(value) => Value::Float(integer_to_float(&Integer::from(*value))?),
            Value::BigInt(_) => {
                let Some((views, new)) = &mut arena else {
                    return Ok(None);
                };
                let Some(integer) = views.integer(a)? else {
                    return Err(raised("type_error", "expected a number"));
                };
                new.read_whole(integer.bits().div_ceil(8))?;
                Value::Float(integer_to_float(&integer)?)
            }
            Value::Float(_) => *a,
            _ => return Err(raised("type_error", "expected a number")),
        },
        Operation::ToInt => match a {
            Value::Float(value) => {
                let integer = float_to_integer(*value)?;
                match (integer.word(), &mut arena) {
                    (Some(word), _) => Value::Int(word),
                    (None, Some((_, new))) => new.integer(integer),
                    (None, None) => return Ok(None),
                }
            }
            Value::Int(_) | Value::BigInt(_) => *a,
            _ => return Err(raised("type_error", "expected a number")),
        },
        Operation::ToText => match a {
            // Digits a short text holds are written nowhere; longer ones
            // are reserved before they are written.
            Value::Int(word) => match short_digits(*word, 10) {
                Some(digits) => Value::Text(digits),
                None => return Ok(None),
            },
            Value::BigInt(_) => return Ok(None),
            Value::Float(value) => {
                let text = value.to_string();
                match &mut arena {
                    Some((_, new)) => new.text(&text),
                    None => match ShortText::new(&text) {
                        Some(short) => Value::Text(short),
                        None => return Ok(None),
                    },
                }
            }
            _ => return Err(raised("type_error", "expected a number")),
        },
        Operation::IntText => {
            // As `ToText`, in the call's radix.
            let Value::Int(word) = a else {
                return Ok(None);
            };
            match short_digits(*word, radix(&args[1])?) {
                Some(digits) => Value::Text(digits),
                None => return Ok(None),
            }
        }
        Operation::IntParse => {
            let radix = radix(&args[1])?;
            let text = match (a, &mut arena) {
                (Value::Text(short), _) => Cow::Borrowed(short.as_str()),
                (Value::LongText(_), Some((views, new))) => match views.text(a)? {
                    Some(text) => {
                        new.read_whole(text.len())?;
                        text.whole()?
                    }
                    None => return Err(raised("type_error", "expected text")),
                },
                (Value::LongText(_), None) => return Ok(None),
                _ => return Err(raised("type_error", "expected text")),
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
            match (integer.word(), &mut arena) {
                (Some(word), _) => Value::Int(word),
                (None, Some((_, new))) => new.integer(integer),
                (None, None) => return Ok(None),
            }
        }
        Operation::FloatParse => {
            let text = match (a, &mut arena) {
                (Value::Text(short), _) => Cow::Borrowed(short.as_str()),
                (Value::LongText(_), Some((views, new))) => match views.text(a)? {
                    Some(text) => {
                        new.read_whole(text.len())?;
                        text.whole()?
                    }
                    None => return Err(raised("type_error", "expected text")),
                },
                (Value::LongText(_), None) => return Ok(None),
                _ => return Err(raised("type_error", "expected text")),
            };
            Value::Float(Float::new(parse_float(&text)?))
        }
    }))
}

/// Every kind with the name `kind` answers for it, in the order of
/// [`ValueKind`]'s variants.
pub(crate) const KIND_NAMES: [(ValueKind, &str); 19] = ValueKind::NAMES;

const _: () = {
    let mut index = 0;
    while index < KIND_NAMES.len() {
        assert!(KIND_NAMES[index].0 as usize == index);
        index += 1;
    }
};

/// A kind's name, as `kind` answers it, from the immutable kind table.
fn kind_name(kind: ValueKind) -> Value {
    crate::short(KIND_NAMES[kind as usize].1)
}

/// A word's digits in `radix`, lower-case and signed as
/// [`integer_text`] spells them, when they fit a short text: one held in
/// the value, which takes no room.
pub(crate) fn short_digits(word: i64, radix: u32) -> Option<ShortText> {
    let mut bytes = [0u8; ShortText::MAX];
    let mut at = bytes.len();
    let mut rest = word.unsigned_abs();
    loop {
        at = at.checked_sub(1)?;
        let digit = char::from_digit(u32::try_from(rest % u64::from(radix)).ok()?, radix)?;
        *bytes.get_mut(at)? = u8::try_from(digit).ok()?;
        rest /= u64::from(radix);
        if rest == 0 {
            break;
        }
    }
    if word < 0 {
        at = at.checked_sub(1)?;
        *bytes.get_mut(at)? = b'-';
    }
    ShortText::ascii(bytes.get(at..)?)
}

/// An integer's digits in `radix`, a text that can be eight times the
/// integer's own bytes (radix 2). The digits are reserved, as the text and
/// as the buffer they are written in, before they are written.
pub(crate) fn integer_text(
    args: &Args<'_>,
    new: &mut Nursery<'_>,
    integer: &Value,
    radix: u32,
) -> Result<Value, NativeError> {
    let Some(integer) = args.integer(integer)? else {
        return Err(raised("type_error", "expected an integer"));
    };
    // An integer below 2^bits has at most bits / log2(radix) + 1 digits;
    // one more digit absorbs the float's rounding, and one is the sign.
    let digits = (integer.bits() as f64 / f64::from(radix).log2()) as u64 + 3;
    new.reserve(0, digits.saturating_mul(2))?;
    new.read_whole(integer.bits().div_ceil(8))?;
    Ok(new.text(&integer.as_bigint().to_str_radix(radix)))
}

fn radix(value: &Value) -> Result<u32, NativeError> {
    match value {
        Value::Int(value) => u32::try_from(*value)
            .ok()
            .filter(|value| (2..=36).contains(value))
            .ok_or_else(|| raised("number_range", "radix must be between 2 and 36")),
        Value::BigInt(_) => Err(raised("number_range", "radix must be between 2 and 36")),
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
        return Err(raised("number_range", "decimal text rounds to infinity"));
    }
    Ok(value)
}

/// The scalar relation of an ordering, shared by general and prepared calls.
#[inline]
fn relation(operation: Operation, order: Option<Ordering>) -> Result<Value, NativeError> {
    Ok(match operation {
        Operation::Compare => Value::Int(match order {
            Some(Ordering::Less) => -1,
            Some(Ordering::Equal) => 0,
            Some(Ordering::Greater) => 1,
            None => return Err(raised("unordered", "NaN has no ordering")),
        }),
        Operation::Eq => Value::Bool(order == Some(Ordering::Equal)),
        Operation::Lt => Value::Bool(order == Some(Ordering::Less)),
        Operation::Le => Value::Bool(matches!(order, Some(Ordering::Less | Ordering::Equal))),
        Operation::Gt => Value::Bool(order == Some(Ordering::Greater)),
        _ => Value::Bool(matches!(order, Some(Ordering::Greater | Ordering::Equal))),
    })
}
