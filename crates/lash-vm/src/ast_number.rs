//! The one rule for number literals in the IR (FIG-3571).
//!
//! A number literal is an IEEE-754 double. Its identity and its stored form
//! follow one rule: `0` and `-0` are distinct values (they print, divide and
//! compare under `Object.is` differently), and every NaN is the one canonical
//! NaN. The stored form is lossless: a finite number is a JSON number, and a
//! non-finite one is the string `"NaN"`, `"Infinity"` or `"-Infinity"`, so a
//! literal decodes to exactly the value that was hashed.

use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// The bits a number literal is identified by: its own bits, except that every
/// NaN is the canonical one.
pub(crate) fn canonical_bits(value: f64) -> u64 {
    if value.is_nan() {
        f64::NAN.to_bits()
    } else {
        value.to_bits()
    }
}

/// A non-finite number literal's stored spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub(crate) enum NonFiniteNumber {
    NaN,
    Infinity,
    #[serde(rename = "-Infinity")]
    NegativeInfinity,
}

/// The stored form of an IR number literal.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub(crate) enum IrNumber {
    Finite(f64),
    NonFinite(NonFiniteNumber),
}

impl From<f64> for IrNumber {
    fn from(value: f64) -> Self {
        if value.is_nan() {
            Self::NonFinite(NonFiniteNumber::NaN)
        } else if value == f64::INFINITY {
            Self::NonFinite(NonFiniteNumber::Infinity)
        } else if value == f64::NEG_INFINITY {
            Self::NonFinite(NonFiniteNumber::NegativeInfinity)
        } else {
            Self::Finite(value)
        }
    }
}

impl From<IrNumber> for f64 {
    fn from(value: IrNumber) -> Self {
        match value {
            IrNumber::Finite(value) => value,
            IrNumber::NonFinite(NonFiniteNumber::NaN) => f64::NAN,
            IrNumber::NonFinite(NonFiniteNumber::Infinity) => f64::INFINITY,
            IrNumber::NonFinite(NonFiniteNumber::NegativeInfinity) => f64::NEG_INFINITY,
        }
    }
}

pub(crate) fn serialize<S: Serializer>(value: &f64, serializer: S) -> Result<S::Ok, S::Error> {
    IrNumber::from(*value).serialize(serializer)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    IrNumber::deserialize(deserializer).map(f64::from)
}
