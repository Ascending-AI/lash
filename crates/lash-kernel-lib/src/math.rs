//! Software binary64 math, pinned to libm 0.2.16 with no architecture feature.
//! Domain failures return NaN, poles and overflow return signed infinities.
//! Dialects that raise for these outcomes check explicitly after the call.

use lash_kernel_doc::{Float, NativeError, Value};

use crate::numeric::as_float;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Math {
    Acos,
    Acosh,
    Asin,
    Asinh,
    Atan,
    Atanh,
    Cbrt,
    Cos,
    Cosh,
    Erf,
    Erfc,
    Exp,
    Exp2,
    Expm1,
    Gamma,
    Lgamma,
    Log,
    Log2,
    Log10,
    Log1p,
    Sin,
    Sinh,
    Sqrt,
    Tan,
    Tanh,
    Atan2,
    Hypot,
    CopySign,
    NextAfter,
    Remainder,
    Pow,
    Fma,
}

impl Math {
    pub(crate) fn arity(self) -> usize {
        match self {
            Self::Atan2
            | Self::Hypot
            | Self::CopySign
            | Self::NextAfter
            | Self::Remainder
            | Self::Pow => 2,
            Self::Fma => 3,
            _ => 1,
        }
    }

    pub(crate) fn call(self, args: &[Value]) -> Result<Value, NativeError> {
        let a = as_float(&args[0])?;
        let b = if self.arity() > 1 {
            as_float(&args[1])?
        } else {
            0.0
        };
        let result = match self {
            Self::Acos => libm::acos(a),
            Self::Acosh => libm::acosh(a),
            Self::Asin => libm::asin(a),
            Self::Asinh => libm::asinh(a),
            Self::Atan => libm::atan(a),
            Self::Atanh => libm::atanh(a),
            Self::Cbrt => libm::cbrt(a),
            Self::Cos => libm::cos(a),
            Self::Cosh => libm::cosh(a),
            Self::Erf => libm::erf(a),
            Self::Erfc => libm::erfc(a),
            Self::Exp => libm::exp(a),
            Self::Exp2 => libm::exp2(a),
            Self::Expm1 => libm::expm1(a),
            Self::Gamma => libm::tgamma(a),
            Self::Lgamma => libm::lgamma(a),
            Self::Log => libm::log(a),
            Self::Log2 => libm::log2(a),
            Self::Log10 => libm::log10(a),
            Self::Log1p => libm::log1p(a),
            Self::Sin => libm::sin(a),
            Self::Sinh => libm::sinh(a),
            Self::Sqrt => libm::sqrt(a),
            Self::Tan => libm::tan(a),
            Self::Tanh => libm::tanh(a),
            Self::Atan2 => libm::atan2(a, b),
            Self::Hypot => libm::hypot(a, b),
            Self::CopySign => libm::copysign(a, b),
            Self::NextAfter => libm::nextafter(a, b),
            Self::Remainder => libm::remainder(a, b),
            Self::Pow => libm::pow(a, b),
            Self::Fma => libm::fma(a, b, as_float(&args[2])?),
        };
        Ok(Value::Float(Float::new(result)))
    }
}
