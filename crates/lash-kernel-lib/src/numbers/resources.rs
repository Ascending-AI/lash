//! Numeric obligations evaluated without executing a native or its nursery.
use lash_kernel_doc::{
    Args, LogicalHeldV1, NativeError, NativeResourceContract as Contract, NativeResourceExit,
    NativeResourceOrigin, Value,
};

use super::{NumericFunction, Operation, arithmetic, plain};
use crate::numeric::Numeric;
use num_bigint::BigInt;
use num_traits::{One, Zero};

fn numeric(args: Args<'_>, value: &Value) -> Result<Numeric, NativeError> {
    match value {
        Value::Float(value) => Ok(Numeric::Float(*value)),
        _ => args
            .integer(value)?
            .map(|value| Numeric::Int(value.into_owned()))
            .ok_or_else(|| super::raised("type_error", "expected a number")),
    }
}

fn number(result: Numeric) -> Contract {
    let payload = match result {
        Numeric::Int(value) => value.bits().div_ceil(8),
        Numeric::Float(_) | Numeric::Bool(_) => 0,
    };
    Contract::value(
        NativeResourceExit::Returned,
        LogicalHeldV1::REFERENCE + payload,
        NativeResourceOrigin::Immutable,
    )
}

pub(super) fn contract(
    native: &NumericFunction,
    args: Args<'_>,
) -> Result<Option<Contract>, NativeError> {
    match evaluate(native, args) {
        Err(NativeError::Refused { kind, message }) => Ok(Contract::refused(kind, &message)),
        answer => answer,
    }
}

fn evaluate(native: &NumericFunction, args: Args<'_>) -> Result<Option<Contract>, NativeError> {
    native.admit(args.values())?;
    let values = args.values();
    // These mathematical functions return numbers without choosing an arena
    // representation or making nursery reservation decisions.
    match native.operation {
        Operation::Binary(op) => {
            let left = numeric(args, &values[0])?;
            let right = numeric(args, &values[1])?;
            return match arithmetic::plain(op, &left, &right)? {
                Some(result) => Ok(Some(number(result))),
                None => match (left, right) {
                    (Numeric::Int(left), Numeric::Int(right)) => power(left, right).map(Some),
                    _ => Ok(None),
                },
            };
        }
        Operation::Unary(op) => {
            return arithmetic::scalar_unary(op, &numeric(args, &values[0])?)
                .map(|value| Some(number(value)));
        }
        Operation::Math(op) => {
            let values = values
                .iter()
                .map(|value| numeric(args, value))
                .collect::<Result<Vec<_>, _>>()?;
            return op.call(&values).map(|value| Some(number(value)));
        }
        Operation::ToInt => {
            return match values[0] {
                Value::Float(value) => {
                    crate::float_to_integer(value).map(|value| Some(number(Numeric::Int(value))))
                }
                _ => Contract::returned(args, &values[0]).map(Some),
            };
        }
        Operation::ToFloat => {
            if let Some(integer) = args.integer(&values[0])? {
                crate::integer_to_float(&integer)?;
            }
            return Ok(Some(number(Numeric::Float(lash_kernel_doc::Float::new(
                0.0,
            )))));
        }
        Operation::ToText | Operation::IntText => {
            let radix = if matches!(native.operation, Operation::IntText) {
                super::radix(&values[1])?
            } else {
                10
            };
            let bytes = match args.integer(&values[0])? {
                Some(integer) => integer.as_bigint().to_str_radix(radix).len(),
                None => match values[0] {
                    Value::Float(value) => value.to_string().len(),
                    _ => return Ok(None),
                },
            };
            return Ok(Some(Contract::value(
                NativeResourceExit::Returned,
                LogicalHeldV1::REFERENCE + bytes as u64,
                NativeResourceOrigin::Immutable,
            )));
        }
        Operation::Eq | Operation::Same => {
            if matches!(native.operation, Operation::Eq) {
                args.equal(&values[0], &values[1])?;
            } else {
                crate::same(&values[0], &values[1], &args)?;
            }
            return Ok(Some(number(Numeric::Bool(false))));
        }
        Operation::Lt | Operation::Le | Operation::Gt | Operation::Ge | Operation::Compare => {
            let order = crate::comparison::compare_reading(
                &values[0],
                &values[1],
                &args,
                |left, right| {
                    crate::numeric::scalar_cmp(&numeric(args, left)?, &numeric(args, right)?)
                },
            )?;
            return Contract::returned(args, &super::relation(native.operation, order)?).map(Some);
        }
        Operation::IntParse => {
            let radix = super::radix(&values[1])?;
            let view = args
                .text(&values[0])?
                .ok_or_else(|| super::raised("type_error", "expected text"))?;
            let text = view.whole()?;
            let digits = text.strip_prefix(['+', '-']).unwrap_or(&text);
            if digits.is_empty()
                || !digits
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() && (byte as char).is_digit(radix))
            {
                return Err(super::raised(
                    "number_parse",
                    "expected signed radix digits without whitespace or separators",
                ));
            }
            let integer = BigInt::parse_bytes(text.as_bytes(), radix)
                .ok_or_else(|| super::raised("number_parse", "invalid integer text"))?;
            return Ok(Some(number(Numeric::Int(lash_kernel_doc::Integer::new(
                integer,
            )))));
        }
        Operation::FloatParse => {
            let view = args
                .text(&values[0])?
                .ok_or_else(|| super::raised("type_error", "expected text"))?;
            super::parse_float(&view.whole()?)?;
            return Ok(Some(number(Numeric::Float(lash_kernel_doc::Float::new(
                0.0,
            )))));
        }
        _ => {}
    }
    match plain(native.operation, values, None)? {
        Some(value) => Contract::returned(args, &value).map(Some),
        None => Ok(None),
    }
}

fn power(
    left: lash_kernel_doc::Integer,
    right: lash_kernel_doc::Integer,
) -> Result<Contract, NativeError> {
    if right.is_negative() {
        return Err(super::raised(
            "number_range",
            "integer exponent must be non-negative",
        ));
    }
    let mut exponent = right.into_bigint();
    let mut base = left.as_bigint().into_owned();
    let mut result = BigInt::one();
    let mut counter = lash_kernel_doc::WorkCounter::new(Some(super::INTEGER_POWER_WORK_LIMIT));
    let products = !exponent.is_zero();
    while !exponent.is_zero() {
        if exponent.bit(0) {
            counter.spend(
                result
                    .bits()
                    .saturating_add(base.bits())
                    .div_ceil(64)
                    .max(1),
            )?;
            result *= &base;
        }
        exponent >>= 1;
        if !exponent.is_zero() {
            counter.spend(base.bits().saturating_mul(2).div_ceil(64).max(1))?;
            base = &base * &base;
        }
    }
    let bits = result.bits().max(left.bits()).max(1);
    let mut contract = number(Numeric::Int(lash_kernel_doc::Integer::new(result)));
    if products {
        // Canonical multiplication owns result, base and product occurrences.
        // Every intermediate magnitude is bounded by the input/result maximum.
        contract
            .events
            .push(lash_kernel_doc::NativeResourceEvent::Scratch {
                owner: lash_kernel_doc::NativeResourceOwner(0),
                bytes: 3 * (LogicalHeldV1::REFERENCE + bits.div_ceil(8)),
                release: lash_kernel_doc::NativeResourceRelease::Exit,
            });
    }
    Ok(contract)
}
