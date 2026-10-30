use serde::de::{Deserialize, Deserializer, MapAccess, Visitor};
use serde::ser::{self, Error as _, Serialize, Serializer};
use serde_json::{Number, Value, value::RawValue};
use std::fmt;

pub enum NumberKind<'a> {
    Unsigned(u64),
    Signed(i64),
    Decimal(&'a Number),
}

pub fn classify(number: &Number) -> NumberKind<'_> {
    if let Some(value) = number.as_u64() {
        NumberKind::Unsigned(value)
    } else if let Some(value) = number.as_i64() {
        NumberKind::Signed(value)
    } else {
        NumberKind::Decimal(number)
    }
}

fn decimal_parts(text: &str) -> Option<(bool, String, i64)> {
    let (negative, text) = text.strip_prefix('-').map_or((false, text), |s| (true, s));
    let (mantissa, exponent) = match text.find(['e', 'E']) {
        Some(index) => (&text[..index], Some(&text[index + 1..])),
        None => (text, None),
    };
    let fractional = mantissa.split_once('.').map_or(0, |(_, tail)| tail.len());
    let mut digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let first = digits.find(|c| c != '0');
    let Some(first) = first else {
        return Some((negative, String::new(), 0));
    };
    let exponent = exponent.map_or(Some(0), |value| value.parse::<i64>().ok())?;
    digits.drain(..first);
    let trimmed = digits.trim_end_matches('0').len();
    let trailing = digits.len() - trimmed;
    digits.truncate(trimmed);
    let exponent = exponent
        .checked_sub(i64::try_from(fractional).ok()?)?
        .checked_add(i64::try_from(trailing).ok()?)?;
    Some((negative, digits, exponent))
}

pub fn is_negative_zero(number: &Number) -> bool {
    decimal_parts(&number.to_string())
        .is_some_and(|(negative, digits, _)| negative && digits.is_empty())
}

fn exact_f64(number: &Number) -> Option<f64> {
    let value = number.as_f64().filter(|value| value.is_finite())?;
    let rendered = Number::from_f64(value)?;
    let original = decimal_parts(&number.to_string())?;
    (Some(original) == decimal_parts(&rendered.to_string())).then_some(value)
}

pub fn serialize<S: Serializer>(number: &Number, serializer: S) -> Result<S::Ok, S::Error> {
    use ser::SerializeMap;
    match classify(number) {
        NumberKind::Unsigned(value) => serializer.serialize_u64(value),
        NumberKind::Signed(value) => serializer.serialize_i64(value),
        NumberKind::Decimal(number) => match exact_f64(number) {
            Some(value) => serializer.serialize_f64(value),
            None => {
                let mut map = serializer.serialize_map(Some(1))?;
                map.serialize_entry("$serde_json::private::Number", &number.to_string())?;
                map.end()
            }
        },
    }
}

pub fn serialize_canonical<S: Serializer>(
    number: &Number,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match classify(number) {
        NumberKind::Unsigned(value) => serializer.serialize_u64(value),
        NumberKind::Signed(value) => serializer.serialize_i64(value),
        NumberKind::Decimal(number) => match exact_f64(number) {
            Some(value) => serializer.serialize_f64(value),
            None => number.serialize(serializer),
        },
    }
}

pub fn deserialize_f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<f64, D::Error> {
    let number = Number::deserialize(deserializer)?;
    number
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| serde::de::Error::custom("JSON number is not a finite f64"))
}

pub fn deserialize_optional_f64<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    Option::<Number>::deserialize(deserializer)?
        .map(|number| {
            number
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| serde::de::Error::custom("JSON number is not a finite f64"))
        })
        .transpose()
}

pub struct Native<T>(pub T);

impl<T: Serialize> Serialize for Native<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(Adapter(serializer))
    }
}

struct Adapter<S>(S);
struct Compound<T>(T);

macro_rules! forward_scalar {
    ($($method:ident($ty:ty)),* $(,)?) => {$(
        fn $method(self, value: $ty) -> Result<Self::Ok, Self::Error> {
            self.0.$method(value)
        }
    )*};
}

impl<S: Serializer> Serializer for Adapter<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    type SerializeSeq = Compound<S::SerializeSeq>;
    type SerializeTuple = Compound<S::SerializeTuple>;
    type SerializeTupleStruct = Compound<S::SerializeTupleStruct>;
    type SerializeTupleVariant = Compound<S::SerializeTupleVariant>;
    type SerializeMap = Compound<S::SerializeMap>;
    type SerializeStruct = Structure<S>;
    type SerializeStructVariant = Compound<S::SerializeStructVariant>;

    forward_scalar!(
        serialize_bool(bool),
        serialize_i8(i8),
        serialize_i16(i16),
        serialize_i32(i32),
        serialize_i64(i64),
        serialize_i128(i128),
        serialize_u8(u8),
        serialize_u16(u16),
        serialize_u32(u32),
        serialize_u64(u64),
        serialize_u128(u128),
        serialize_f32(f32),
        serialize_f64(f64),
        serialize_char(char),
        serialize_str(&str),
        serialize_bytes(&[u8])
    );

    fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_none()
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_some(&Native(value))
    }
    fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit()
    }
    fn serialize_unit_struct(self, name: &'static str) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_struct(name)
    }
    fn serialize_unit_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_unit_variant(name, index, variant)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0.serialize_newtype_struct(name, &Native(value))
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<Self::Ok, Self::Error> {
        self.0
            .serialize_newtype_variant(name, index, variant, &Native(value))
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        self.0.serialize_seq(len).map(Compound)
    }
    fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        self.0.serialize_tuple(len).map(Compound)
    }
    fn serialize_tuple_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        self.0.serialize_tuple_struct(name, len).map(Compound)
    }
    fn serialize_tuple_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        self.0
            .serialize_tuple_variant(name, index, variant, len)
            .map(Compound)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        self.0.serialize_map(len).map(Compound)
    }
    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        match name {
            "$serde_json::private::Number" => Ok(Structure::Number {
                target: self.0,
                fields: serde_json::value::Serializer
                    .serialize_struct(name, len)
                    .map_err(S::Error::custom)?,
            }),
            "$serde_json::private::RawValue" => Ok(Structure::Raw {
                target: self.0,
                text: None,
            }),
            _ => self.0.serialize_struct(name, len).map(Structure::Ordinary),
        }
    }
    fn serialize_struct_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        self.0
            .serialize_struct_variant(name, index, variant, len)
            .map(Compound)
    }
    fn collect_str<T: ?Sized + fmt::Display>(self, value: &T) -> Result<Self::Ok, Self::Error> {
        self.0.collect_str(value)
    }
    fn is_human_readable(&self) -> bool {
        self.0.is_human_readable()
    }
}

macro_rules! sequence {
    ($trait:ident, $method:ident) => {
        impl<T: ser::$trait> ser::$trait for Compound<T> {
            type Ok = T::Ok;
            type Error = T::Error;
            fn $method<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<(), Self::Error> {
                self.0.$method(&Native(value))
            }
            fn end(self) -> Result<Self::Ok, Self::Error> {
                self.0.end()
            }
        }
    };
}
sequence!(SerializeSeq, serialize_element);
sequence!(SerializeTuple, serialize_element);
sequence!(SerializeTupleStruct, serialize_field);
sequence!(SerializeTupleVariant, serialize_field);

impl<T: ser::SerializeMap> ser::SerializeMap for Compound<T> {
    type Ok = T::Ok;
    type Error = T::Error;
    fn serialize_key<K: ?Sized + Serialize>(&mut self, key: &K) -> Result<(), Self::Error> {
        self.0.serialize_key(&Native(key))
    }
    fn serialize_value<V: ?Sized + Serialize>(&mut self, value: &V) -> Result<(), Self::Error> {
        self.0.serialize_value(&Native(value))
    }
    fn serialize_entry<K: ?Sized + Serialize, V: ?Sized + Serialize>(
        &mut self,
        key: &K,
        value: &V,
    ) -> Result<(), Self::Error> {
        self.0.serialize_entry(&Native(key), &Native(value))
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

impl<T: ser::SerializeStructVariant> ser::SerializeStructVariant for Compound<T> {
    type Ok = T::Ok;
    type Error = T::Error;
    fn serialize_field<V: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &V,
    ) -> Result<(), Self::Error> {
        self.0.serialize_field(key, &Native(value))
    }
    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        self.0.skip_field(key)
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        self.0.end()
    }
}

enum Structure<S: Serializer> {
    Ordinary(S::SerializeStruct),
    Number {
        target: S,
        fields: <serde_json::value::Serializer as Serializer>::SerializeStruct,
    },
    Raw {
        target: S,
        text: Option<String>,
    },
}

impl<S: Serializer> ser::SerializeStruct for Structure<S> {
    type Ok = S::Ok;
    type Error = S::Error;
    fn serialize_field<V: ?Sized + Serialize>(
        &mut self,
        key: &'static str,
        value: &V,
    ) -> Result<(), Self::Error> {
        match self {
            Self::Ordinary(fields) => fields.serialize_field(key, &Native(value)),
            Self::Number { fields, .. } => {
                fields.serialize_field(key, value).map_err(S::Error::custom)
            }
            Self::Raw { text, .. } => {
                if key != "$serde_json::private::RawValue" || text.is_some() {
                    return Err(S::Error::custom("invalid raw JSON field"));
                }
                match serde_json::to_value(value).map_err(S::Error::custom)? {
                    Value::String(value) => {
                        *text = Some(value);
                        Ok(())
                    }
                    _ => Err(S::Error::custom("invalid raw JSON text")),
                }
            }
        }
    }
    fn skip_field(&mut self, key: &'static str) -> Result<(), Self::Error> {
        match self {
            Self::Ordinary(fields) => fields.skip_field(key),
            _ => Err(S::Error::custom("invalid JSON scalar field")),
        }
    }
    fn end(self) -> Result<Self::Ok, Self::Error> {
        match self {
            Self::Ordinary(fields) => fields.end(),
            Self::Number { target, fields } => match fields.end().map_err(S::Error::custom)? {
                Value::Number(number) => serialize(&number, target),
                _ => Err(S::Error::custom("invalid JSON number")),
            },
            Self::Raw { target, text } => {
                let text = text.ok_or_else(|| S::Error::custom("missing raw JSON text"))?;
                let raw: &RawValue = serde_json::from_str(&text).map_err(S::Error::custom)?;
                Raw(raw).serialize(target)
            }
        }
    }
}

struct Raw<'a>(&'a RawValue);

impl Serialize for Raw<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use ser::{SerializeMap, SerializeSeq};
        let text = self.0.get();
        match text.trim_start().as_bytes().first() {
            Some(b'{') => {
                let entries: RawEntries<'_> =
                    serde_json::from_str(text).map_err(S::Error::custom)?;
                let mut map = serializer.serialize_map(Some(entries.0.len()))?;
                for (key, value) in entries.0 {
                    map.serialize_entry(&key, &Raw(value))?;
                }
                map.end()
            }
            Some(b'[') => {
                let values: Vec<&RawValue> =
                    serde_json::from_str(text).map_err(S::Error::custom)?;
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(&Raw(value))?;
                }
                sequence.end()
            }
            Some(b'"') => {
                let value: String = serde_json::from_str(text).map_err(S::Error::custom)?;
                serializer.serialize_str(&value)
            }
            Some(b't') => serializer.serialize_bool(true),
            Some(b'f') => serializer.serialize_bool(false),
            Some(b'n') => serializer.serialize_unit(),
            _ => {
                let number: Number = serde_json::from_str(text).map_err(S::Error::custom)?;
                serialize(&number, serializer)
            }
        }
    }
}

struct RawEntries<'a>(Vec<(String, &'a RawValue)>);
impl<'de> Deserialize<'de> for RawEntries<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct EntriesVisitor;
        impl<'de> Visitor<'de> for EntriesVisitor {
            type Value = RawEntries<'de>;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a JSON object")
            }
            fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
                let mut entries = Vec::new();
                while let Some(entry) = map.next_entry()? {
                    entries.push(entry);
                }
                Ok(RawEntries(entries))
            }
        }
        deserializer.deserialize_map(EntriesVisitor)
    }
}
