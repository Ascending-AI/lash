//! The two number kinds, and the number token an effect result carries.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::name::string_schema;

/// An arbitrary-precision integer. Its stored form is its decimal spelling.
///
/// Cloning is cheap: an integer that fits in 64 bits is held in place, and a
/// larger one is shared.
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Integer(Repr);

/// An integer's one representation: `Big` holds only what `Small` cannot,
/// so two equal integers are always the same variant.
#[derive(Clone, PartialEq, Eq, Hash)]
enum Repr {
    Small(i64),
    Big(Arc<BigInt>),
}

string_schema!(Integer, "Integer", "^(0|-?[1-9][0-9]*)$");

/// A text that is not an integer's canonical decimal spelling.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not an integer: write decimal digits with an optional leading `-`")]
pub struct InvalidInteger {
    pub text: String,
}

impl Integer {
    pub fn new(value: impl Into<BigInt>) -> Self {
        let value = value.into();
        Self(match value.to_i64() {
            Some(small) => Repr::Small(small),
            None => Repr::Big(Arc::new(value)),
        })
    }

    /// How many bits the integer's magnitude takes; zero takes none.
    pub fn bits(&self) -> u64 {
        match &self.0 {
            Repr::Small(small) => u64::from(64 - small.unsigned_abs().leading_zeros()),
            Repr::Big(big) => big.bits(),
        }
    }

    pub fn is_negative(&self) -> bool {
        match &self.0 {
            Repr::Small(small) => *small < 0,
            Repr::Big(big) => big.sign() == Sign::Minus,
        }
    }

    /// The integer as a `BigInt`, which one that fits in 64 bits is built
    /// as.
    pub fn as_bigint(&self) -> Cow<'_, BigInt> {
        match &self.0 {
            Repr::Small(small) => Cow::Owned(BigInt::from(*small)),
            Repr::Big(big) => Cow::Borrowed(big),
        }
    }

    pub fn into_bigint(self) -> BigInt {
        match self.0 {
            Repr::Small(small) => BigInt::from(small),
            Repr::Big(big) => Arc::unwrap_or_clone(big),
        }
    }

    /// Reads the canonical decimal spelling: no `+`, no leading zero, no
    /// `-0`.
    pub fn parse(text: &str) -> Result<Self, InvalidInteger> {
        let digits = text.strip_prefix('-').unwrap_or(text);
        let canonical = !digits.is_empty()
            && digits.bytes().all(|digit| digit.is_ascii_digit())
            && (digits == "0" && digits.len() == text.len() || !digits.starts_with('0'));
        let invalid = || InvalidInteger {
            text: text.to_string(),
        };
        if !canonical {
            return Err(invalid());
        }
        match text.parse::<i64>() {
            Ok(small) => Ok(Self(Repr::Small(small))),
            Err(_) => BigInt::from_str(text).map(Self::new).map_err(|_| invalid()),
        }
    }
}

impl ToPrimitive for Integer {
    fn to_i64(&self) -> Option<i64> {
        match &self.0 {
            Repr::Small(small) => Some(*small),
            Repr::Big(_) => None,
        }
    }

    fn to_u64(&self) -> Option<u64> {
        match &self.0 {
            Repr::Small(small) => u64::try_from(*small).ok(),
            Repr::Big(big) => big.to_u64(),
        }
    }

    fn to_i128(&self) -> Option<i128> {
        match &self.0 {
            Repr::Small(small) => Some(i128::from(*small)),
            Repr::Big(big) => big.to_i128(),
        }
    }

    fn to_u128(&self) -> Option<u128> {
        match &self.0 {
            Repr::Small(small) => u128::try_from(*small).ok(),
            Repr::Big(big) => big.to_u128(),
        }
    }

    fn to_f64(&self) -> Option<f64> {
        match &self.0 {
            Repr::Small(small) => small.to_f64(),
            Repr::Big(big) => big.to_f64(),
        }
    }
}

impl Ord for Integer {
    fn cmp(&self, other: &Self) -> Ordering {
        match (&self.0, &other.0) {
            (Repr::Small(a), Repr::Small(b)) => a.cmp(b),
            _ => self.as_bigint().cmp(&other.as_bigint()),
        }
    }
}

impl PartialOrd for Integer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Integer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Repr::Small(small) => small.fmt(f),
            Repr::Big(big) => big.fmt(f),
        }
    }
}

impl fmt::Debug for Integer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Integer")
            .field(&format_args!("{self}"))
            .finish()
    }
}

impl From<BigInt> for Integer {
    fn from(value: BigInt) -> Self {
        Self::new(value)
    }
}

impl From<i64> for Integer {
    fn from(value: i64) -> Self {
        Self(Repr::Small(value))
    }
}

impl TryFrom<String> for Integer {
    type Error = InvalidInteger;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Integer> for String {
    fn from(value: Integer) -> Self {
        value.to_string()
    }
}

/// A 64-bit IEEE float as data.
///
/// As data a float is its bits, with every NaN the one canonical NaN: two
/// `Float`s are the same datum exactly when they print the same, so `-0.0`
/// and `0.0` are different data. That is the identity of a literal, not the
/// kernel's `eq` (`K-VAL-020` onwards), under which `-0.0` equals `0.0` and
/// NaN equals nothing. Its stored form is its kernel text spelling
/// (`K-VAL-031`).
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Float(f64);

string_schema!(
    Float,
    "Float",
    r"^(nan|-?inf|-?(0|[1-9][0-9]*)\.[0-9]+|-?[1-9](\.[0-9]+)?e-?[1-9][0-9]*)$"
);

/// A text that is not a float's canonical spelling.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not a float's canonical spelling")]
pub struct InvalidFloat {
    pub text: String,
}

impl Float {
    pub fn new(value: f64) -> Self {
        Self(if value.is_nan() { f64::NAN } else { value })
    }

    pub fn get(self) -> f64 {
        self.0
    }

    /// Reads the canonical spelling [`Float`]'s `Display` writes, and only
    /// that: a spelling that would print differently is refused.
    pub fn parse(text: &str) -> Result<Self, InvalidFloat> {
        let value = match text {
            "nan" => Some(f64::NAN),
            "inf" => Some(f64::INFINITY),
            "-inf" => Some(f64::NEG_INFINITY),
            _ if text
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'.' | b'e' | b'-')) =>
            {
                f64::from_str(text).ok()
            }
            _ => None,
        };
        value
            .map(Self::new)
            .filter(|float| float.to_string() == text)
            .ok_or_else(|| InvalidFloat {
                text: text.to_string(),
            })
    }
}

impl fmt::Display for Float {
    /// `K-VAL-031`: the shortest digits that read back as the same float,
    /// laid out as a plain decimal with at least one fractional digit when
    /// `1e-4 <= |x| < 1e16`, and as `d[.ddd]e<exponent>` otherwise.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = self.0;
        if value.is_nan() {
            return f.write_str("nan");
        }
        if value.is_infinite() {
            return f.write_str(if value > 0.0 { "inf" } else { "-inf" });
        }
        if value == 0.0 {
            return f.write_str(if value.is_sign_negative() {
                "-0.0"
            } else {
                "0.0"
            });
        }
        // `{:e}` writes the shortest round-trip digits as `d[.ddd]e<exp>`.
        let scientific = format!("{:e}", value.abs());
        let Some((mantissa, exponent)) = scientific.split_once('e') else {
            return Err(fmt::Error);
        };
        let exponent: i32 = exponent.parse().map_err(|_| fmt::Error)?;
        if value < 0.0 {
            f.write_str("-")?;
        }
        if !(-4..16).contains(&exponent) {
            return write!(f, "{mantissa}e{exponent}");
        }
        let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
        if exponent < 0 {
            let zeros = "0".repeat(usize::try_from(-exponent - 1).map_err(|_| fmt::Error)?);
            return write!(f, "0.{zeros}{digits}");
        }
        let whole = usize::try_from(exponent).map_err(|_| fmt::Error)? + 1;
        if digits.len() <= whole {
            let zeros = "0".repeat(whole - digits.len());
            write!(f, "{digits}{zeros}.0")
        } else {
            write!(f, "{}.{}", &digits[..whole], &digits[whole..])
        }
    }
}

impl fmt::Debug for Float {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Float({self})")
    }
}

impl PartialEq for Float {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_bits() == other.0.to_bits()
    }
}

impl Eq for Float {}

impl std::hash::Hash for Float {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

impl From<f64> for Float {
    fn from(value: f64) -> Self {
        Self::new(value)
    }
}

impl TryFrom<String> for Float {
    type Error = InvalidFloat;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text)
    }
}

impl From<Float> for String {
    fn from(value: Float) -> Self {
        value.to_string()
    }
}

/// How a document's effect results decode a number whose type says only
/// "number" (`K-EFF-006`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum NumberPolicy {
    /// Every bare number is a float.
    Float,
    /// A number written with no fraction and no exponent is an integer; any
    /// other is a float.
    BySpelling,
}

/// A number as an effect result spells it: a JSON number token, carried
/// undecoded so that no digit is lost before the kernel decides its kind
/// (`K-EFF-005`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct NumberToken(String);

string_schema!(
    NumberToken,
    "NumberToken",
    r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?$"
);

/// A text that is not a JSON number.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not a JSON number")]
pub struct InvalidNumberToken {
    pub text: String,
}

impl NumberToken {
    /// Accepts exactly the JSON number grammar (RFC 8259 §6).
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidNumberToken> {
        let text = text.into();
        if is_json_number(&text) {
            Ok(Self(text))
        } else {
            Err(InvalidNumberToken { text })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the token has neither a fraction nor an exponent: the
    /// spelling [`NumberPolicy::BySpelling`] decodes as an integer.
    pub fn is_integer_spelling(&self) -> bool {
        !self.0.contains(['.', 'e', 'E'])
    }
}

impl TryFrom<String> for NumberToken {
    type Error = InvalidNumberToken;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<NumberToken> for String {
    fn from(token: NumberToken) -> Self {
        token.0
    }
}

fn is_json_number(text: &str) -> bool {
    let rest = text.strip_prefix('-').unwrap_or(text);
    let whole_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    let (whole, mut rest) = rest.split_at(whole_len);
    if whole.is_empty() || (whole.len() > 1 && whole.starts_with('0')) {
        return false;
    }
    if let Some(fraction) = rest.strip_prefix('.') {
        let len = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if len == 0 {
            return false;
        }
        rest = &fraction[len..];
    }
    if let Some(exponent) = rest.strip_prefix(['e', 'E']) {
        let digits = exponent.strip_prefix(['+', '-']).unwrap_or(exponent);
        return !digits.is_empty() && digits.bytes().all(|digit| digit.is_ascii_digit());
    }
    rest.is_empty()
}
