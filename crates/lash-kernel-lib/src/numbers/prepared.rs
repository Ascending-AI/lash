//! Library-owned entries specialized by operation and operand domain.

use lash_kernel_doc::PreparedAnswer;

use super::{Binary, Domain, Math, Operation, PreparedNative, Unary, Value, plain};

trait Scalar {
    const OPERATION: Operation;
    const ARITY: usize;
}

#[inline]
fn run<const DOMAIN: u8, const CHARGED: bool, S: Scalar>(args: &[Value]) -> Option<PreparedAnswer> {
    if args.len() != S::ARITY {
        return None;
    }
    if DOMAIN != 2
        && let [Value::Float(a), Value::Float(b)] = args
    {
        let value = match S::OPERATION {
            Operation::Binary(op) if !matches!(op, Binary::Min | Binary::Max) => Some(
                Value::Float(crate::arithmetic::float_binary(op, a.get(), b.get())),
            ),
            Operation::Eq
            | Operation::Lt
            | Operation::Le
            | Operation::Gt
            | Operation::Ge
            | Operation::Compare => {
                Some(super::relation(S::OPERATION, crate::numeric::float_cmp(*a, *b)).ok()?)
            }
            _ => None,
        };
        if let Some(value) = value {
            return Some(PreparedAnswer {
                value,
                scalar_sizes: CHARGED
                    .then_some(u8::from(matches!(value, Value::Int(n) if n != 0)) << 3),
            });
        }
    }
    let mut sizes = CHARGED.then_some(0u8);
    if matches!(S::OPERATION, Operation::IntParse | Operation::FloatParse) {
        if !matches!(args[0], Value::Text(_))
            || (S::ARITY == 2 && !matches!(args[1], Value::Int(_)))
        {
            return None;
        }
        sizes = None;
    } else if DOMAIN != 0 || CHARGED {
        for (index, value) in args.iter().enumerate() {
            match value {
                Value::Int(word) => {
                    if DOMAIN == 3 {
                        return None;
                    }
                    if CHARGED && *word != 0 {
                        sizes = sizes.map(|mask| mask | (1 << index));
                    }
                }
                Value::Float(_) => {
                    if DOMAIN == 2 {
                        return None;
                    }
                }
                Value::Null | Value::Absent | Value::Bool(_) => {
                    if DOMAIN != 0 {
                        return None;
                    }
                }
                _ => {
                    if DOMAIN != 0 {
                        return None;
                    }
                    sizes = None;
                }
            }
        }
    }
    // Errors decline to the general entry, which charges the raise using
    // the original formula and its absent result measurement.
    let value = plain(S::OPERATION, args, None).ok()??;
    let result_size = if CHARGED {
        match value {
            Value::Int(word) => Some(word != 0),
            Value::Null | Value::Absent | Value::Bool(_) | Value::Float(_) => Some(false),
            _ => None,
        }
    } else {
        None
    };
    Some(PreparedAnswer {
        value,
        scalar_sizes: if CHARGED {
            sizes
                .zip(result_size)
                .map(|(mask, nonzero)| mask | (u8::from(nonzero) << 3))
        } else {
            None
        },
    })
}

macro_rules! entries {
    ($( $marker:ident, $operation:pat => $expression:expr, $arity:literal; )*) => {
        $(struct $marker;
        impl Scalar for $marker {
            const OPERATION: Operation = $expression;
            const ARITY: usize = $arity;
        })*

        pub(super) fn entry(operation: Operation, domain: Domain) -> Option<PreparedNative> {
            // Integer powers always count work, even when the result fits
            // in a word; there is no heap-independent, uncounted answer.
            if matches!((operation, domain), (Operation::Binary(Binary::Pow), Domain::Int)) {
                return None;
            }
            Some(match operation {
                $( $operation => match domain {
                    Domain::Any => PreparedNative { entry: |args| run::<0, false, $marker>(args).map(|answer| answer.value), charged_entry: Some(run::<0, true, $marker>), operation: concat!(stringify!($expression), " / Any") },
                    Domain::Number => PreparedNative { entry: |args| run::<1, false, $marker>(args).map(|answer| answer.value), charged_entry: Some(run::<1, true, $marker>), operation: concat!(stringify!($expression), " / Number") },
                    Domain::Int => PreparedNative { entry: |args| run::<2, false, $marker>(args).map(|answer| answer.value), charged_entry: Some(run::<2, true, $marker>), operation: concat!(stringify!($expression), " / Int") },
                    Domain::Float => PreparedNative { entry: |args| run::<3, false, $marker>(args).map(|answer| answer.value), charged_entry: Some(run::<3, true, $marker>), operation: concat!(stringify!($expression), " / Float") },
                }, )*
            })
        }
    };
}

entries! {
    BAdd, Operation::Binary(Binary::Add) => Operation::Binary(Binary::Add), 2;
    BSub, Operation::Binary(Binary::Sub) => Operation::Binary(Binary::Sub), 2;
    BMul, Operation::Binary(Binary::Mul) => Operation::Binary(Binary::Mul), 2;
    BDiv, Operation::Binary(Binary::Div) => Operation::Binary(Binary::Div), 2;
    BDivFloor, Operation::Binary(Binary::DivFloor) => Operation::Binary(Binary::DivFloor), 2;
    BDivTrunc, Operation::Binary(Binary::DivTrunc) => Operation::Binary(Binary::DivTrunc), 2;
    BRemFloor, Operation::Binary(Binary::RemFloor) => Operation::Binary(Binary::RemFloor), 2;
    BRemTrunc, Operation::Binary(Binary::RemTrunc) => Operation::Binary(Binary::RemTrunc), 2;
    BPow, Operation::Binary(Binary::Pow) => Operation::Binary(Binary::Pow), 2;
    BMin, Operation::Binary(Binary::Min) => Operation::Binary(Binary::Min), 2;
    BMax, Operation::Binary(Binary::Max) => Operation::Binary(Binary::Max), 2;
    UNeg, Operation::Unary(Unary::Neg) => Operation::Unary(Unary::Neg), 1;
    UAbs, Operation::Unary(Unary::Abs) => Operation::Unary(Unary::Abs), 1;
    UFloor, Operation::Unary(Unary::Floor) => Operation::Unary(Unary::Floor), 1;
    UCeil, Operation::Unary(Unary::Ceil) => Operation::Unary(Unary::Ceil), 1;
    UTrunc, Operation::Unary(Unary::Trunc) => Operation::Unary(Unary::Trunc), 1;
    URoundEven, Operation::Unary(Unary::RoundEven) => Operation::Unary(Unary::RoundEven), 1;
    URoundAway, Operation::Unary(Unary::RoundAway) => Operation::Unary(Unary::RoundAway), 1;
    URoundUp, Operation::Unary(Unary::RoundUp) => Operation::Unary(Unary::RoundUp), 1;
    USign, Operation::Unary(Unary::Sign) => Operation::Unary(Unary::Sign), 1;
    UIsFinite, Operation::Unary(Unary::IsFinite) => Operation::Unary(Unary::IsFinite), 1;
    UIsInfinite, Operation::Unary(Unary::IsInfinite) => Operation::Unary(Unary::IsInfinite), 1;
    UIsNan, Operation::Unary(Unary::IsNan) => Operation::Unary(Unary::IsNan), 1;
    UIsInteger, Operation::Unary(Unary::IsInteger) => Operation::Unary(Unary::IsInteger), 1;
    Eq, Operation::Eq => Operation::Eq, 2;
    Same, Operation::Same => Operation::Same, 2;
    Lt, Operation::Lt => Operation::Lt, 2;
    Le, Operation::Le => Operation::Le, 2;
    Gt, Operation::Gt => Operation::Gt, 2;
    Ge, Operation::Ge => Operation::Ge, 2;
    Compare, Operation::Compare => Operation::Compare, 2;
    Ref, Operation::Ref => Operation::Ref, 1;
    Kind, Operation::Kind => Operation::Kind, 1;
    ToFloat, Operation::ToFloat => Operation::ToFloat, 1;
    ToInt, Operation::ToInt => Operation::ToInt, 1;
    ToText, Operation::ToText => Operation::ToText, 1;
    IntText, Operation::IntText => Operation::IntText, 2;
    IntParse, Operation::IntParse => Operation::IntParse, 2;
    FloatParse, Operation::FloatParse => Operation::FloatParse, 1;
    MAcos, Operation::Math(Math::Acos) => Operation::Math(Math::Acos), 1;
    MAcosh, Operation::Math(Math::Acosh) => Operation::Math(Math::Acosh), 1;
    MAsin, Operation::Math(Math::Asin) => Operation::Math(Math::Asin), 1;
    MAsinh, Operation::Math(Math::Asinh) => Operation::Math(Math::Asinh), 1;
    MAtan, Operation::Math(Math::Atan) => Operation::Math(Math::Atan), 1;
    MAtanh, Operation::Math(Math::Atanh) => Operation::Math(Math::Atanh), 1;
    MCbrt, Operation::Math(Math::Cbrt) => Operation::Math(Math::Cbrt), 1;
    MCos, Operation::Math(Math::Cos) => Operation::Math(Math::Cos), 1;
    MCosh, Operation::Math(Math::Cosh) => Operation::Math(Math::Cosh), 1;
    MErf, Operation::Math(Math::Erf) => Operation::Math(Math::Erf), 1;
    MErfc, Operation::Math(Math::Erfc) => Operation::Math(Math::Erfc), 1;
    MExp, Operation::Math(Math::Exp) => Operation::Math(Math::Exp), 1;
    MExp2, Operation::Math(Math::Exp2) => Operation::Math(Math::Exp2), 1;
    MExpm1, Operation::Math(Math::Expm1) => Operation::Math(Math::Expm1), 1;
    MGamma, Operation::Math(Math::Gamma) => Operation::Math(Math::Gamma), 1;
    MLgamma, Operation::Math(Math::Lgamma) => Operation::Math(Math::Lgamma), 1;
    MLog, Operation::Math(Math::Log) => Operation::Math(Math::Log), 1;
    MLog2, Operation::Math(Math::Log2) => Operation::Math(Math::Log2), 1;
    MLog10, Operation::Math(Math::Log10) => Operation::Math(Math::Log10), 1;
    MLog1p, Operation::Math(Math::Log1p) => Operation::Math(Math::Log1p), 1;
    MSin, Operation::Math(Math::Sin) => Operation::Math(Math::Sin), 1;
    MSinh, Operation::Math(Math::Sinh) => Operation::Math(Math::Sinh), 1;
    MSqrt, Operation::Math(Math::Sqrt) => Operation::Math(Math::Sqrt), 1;
    MTan, Operation::Math(Math::Tan) => Operation::Math(Math::Tan), 1;
    MTanh, Operation::Math(Math::Tanh) => Operation::Math(Math::Tanh), 1;
    MAtan2, Operation::Math(Math::Atan2) => Operation::Math(Math::Atan2), 2;
    MHypot, Operation::Math(Math::Hypot) => Operation::Math(Math::Hypot), 2;
    MCopySign, Operation::Math(Math::CopySign) => Operation::Math(Math::CopySign), 2;
    MNextAfter, Operation::Math(Math::NextAfter) => Operation::Math(Math::NextAfter), 2;
    MRemainder, Operation::Math(Math::Remainder) => Operation::Math(Math::Remainder), 2;
    MPow, Operation::Math(Math::Pow) => Operation::Math(Math::Pow), 2;
    MFma, Operation::Math(Math::Fma) => Operation::Math(Math::Fma), 3;
}
