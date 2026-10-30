use std::{cell::Cell, fmt, sync::OnceLock};

use serde::Deserialize;
use serde::de::{Deserializer, Visitor};
use serde_json::{Number, value::RawValue};

use super::{NumberKind, classify};

mod adapter;

const NUMBER_TOKEN: &str = "$serde_json::private::Number";
const VALUE_MARKER: &str = "$lash::json_number::value";

#[derive(Debug, thiserror::Error)]
pub enum DecodeError {
    #[error("MessagePack decode: {0}")]
    MessagePack(#[from] rmp_serde::decode::Error),
    #[error("JSON number is outside this build's domain: {spelling}")]
    NumberOutsideDomain { spelling: String },
    #[error("trailing bytes after MessagePack value")]
    TrailingBytes,
}

pub fn decode<'de, T: Deserialize<'de>>(bytes: &'de [u8]) -> Result<T, DecodeError> {
    super::walk::validate(bytes)?;
    let refusal = Cell::new(None);
    let mut deserializer = rmp_serde::Deserializer::from_read_ref(bytes);
    match T::deserialize(adapter::Adapter::new(&mut deserializer, &refusal)) {
        Ok(value) => Ok(value),
        Err(error) => match refusal.take() {
            Some(spelling) => Err(DecodeError::NumberOutsideDomain { spelling }),
            None => Err(DecodeError::MessagePack(error)),
        },
    }
}

pub fn deserialize_value<'de, D, V>(deserializer: D, visitor: V) -> Result<V::Value, D::Error>
where
    D: Deserializer<'de>,
    V: Visitor<'de>,
{
    deserializer.deserialize_newtype_struct(VALUE_MARKER, TransparentVisitor(visitor))
}

struct TransparentVisitor<V>(V);

impl<'de, V: Visitor<'de>> Visitor<'de> for TransparentVisitor<V> {
    type Value = V::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.expecting(formatter)
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        deserializer.deserialize_any(self.0)
    }
}

fn number_lexeme(text: &str) -> bool {
    matches!(text.as_bytes().first(), Some(b'-' | b'0'..=b'9'))
        && serde_json::from_str::<&RawValue>(text).is_ok_and(|raw| raw.get() == text)
}

pub fn parse_exact_number(text: &str) -> Option<Number> {
    if !number_lexeme(text) {
        return None;
    }
    let number: Number = text.parse().ok()?;
    (number.to_string() == text).then_some(number)
}

fn exact_numbers_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| parse_exact_number("1.00000000000000000001").is_some())
}

fn decimal_text<'a, E: serde::de::Error>(
    bytes: &'a [u8],
    refusal: &Cell<Option<String>>,
) -> Result<&'a str, E> {
    let text = std::str::from_utf8(bytes).map_err(E::custom)?;
    if !number_lexeme(text) || text == "-0" {
        return Err(E::custom("invalid decimal extension"));
    }
    let number = parse_exact_number(text);
    if let Some(number) = &number
        && !matches!(classify(number), NumberKind::Decimal(_))
    {
        return Err(E::custom("native number encoded as decimal extension"));
    }
    if !exact_numbers_supported() {
        refusal.set(Some(text.to_owned()));
        return Err(E::custom("number outside this build's domain"));
    }
    number.ok_or_else(|| E::custom("noncanonical decimal extension"))?;
    Ok(text)
}
