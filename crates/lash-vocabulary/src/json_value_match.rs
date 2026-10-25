//! Compare a serde value with an existing JSON tree without constructing a
//! second tree. This has serde_json::to_value's value semantics, including
//! last-write-wins object keys and JSON's numeric representations.
use serde::{Serialize, ser};
use serde_json::{Map, Value};
use std::collections::BTreeMap;

type Error = serde_json::Error;
type Result<T> = std::result::Result<T, Error>;

pub fn matches<T: Serialize + ?Sized>(value: &T, expected: &Value) -> bool {
    value.serialize(Compare(Some(expected))).unwrap_or(false)
}
struct Compare<'a>(Option<&'a Value>);
impl<'a> Compare<'a> {
    fn scalar<T: Serialize>(self, value: T) -> Result<bool> {
        Ok(self.0 == Some(&serde_json::to_value(value)?))
    }
    fn variant(self, name: &str) -> Self {
        Self(
            self.0
                .and_then(Value::as_object)
                .filter(|map| map.len() == 1)
                .and_then(|map| map.get(name)),
        )
    }
    fn sequence(self) -> Sequence<'a> {
        Sequence {
            expected: self.0.and_then(Value::as_array),
            at: 0,
            matches: true,
        }
    }
    fn object(self) -> Object<'a> {
        Object {
            expected: self.0.and_then(Value::as_object),
            seen: BTreeMap::new(),
            pending: None,
            matches: true,
        }
    }
}
macro_rules! scalar {
    ($($name:ident($ty:ty)),* $(,)?) => {$(fn $name(self, value: $ty) -> Result<bool> { self.scalar(value) })*};
}
impl<'a> ser::Serializer for Compare<'a> {
    type Ok = bool;
    type Error = Error;
    type SerializeSeq = Sequence<'a>;
    type SerializeTuple = Sequence<'a>;
    type SerializeTupleStruct = Sequence<'a>;
    type SerializeTupleVariant = Sequence<'a>;
    type SerializeMap = Object<'a>;
    type SerializeStruct = Object<'a>;
    type SerializeStructVariant = Object<'a>;
    scalar!(
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
        serialize_f64(f64)
    );
    fn serialize_char(self, value: char) -> Result<bool> {
        self.serialize_str(value.encode_utf8(&mut [0; 4]))
    }
    fn serialize_str(self, value: &str) -> Result<bool> {
        Ok(self.0.and_then(Value::as_str) == Some(value))
    }
    fn serialize_bytes(self, value: &[u8]) -> Result<bool> {
        let mut sequence = self.sequence();
        for byte in value {
            sequence.element(byte)?;
        }
        sequence.finish()
    }
    fn serialize_none(self) -> Result<bool> {
        self.serialize_unit()
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<bool> {
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<bool> {
        Ok(self.0.is_some_and(Value::is_null))
    }
    fn serialize_unit_struct(self, _: &'static str) -> Result<bool> {
        self.serialize_unit()
    }
    fn serialize_unit_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
    ) -> Result<bool> {
        self.serialize_str(variant)
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        value: &T,
    ) -> Result<bool> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<bool> {
        value.serialize(self.variant(variant))
    }
    fn serialize_seq(self, _: Option<usize>) -> Result<Self::SerializeSeq> {
        Ok(self.sequence())
    }
    fn serialize_tuple(self, _: usize) -> Result<Self::SerializeTuple> {
        Ok(self.sequence())
    }
    fn serialize_tuple_struct(
        self,
        _: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleStruct> {
        Ok(self.sequence())
    }
    fn serialize_tuple_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Self::SerializeTupleVariant> {
        Ok(self.variant(variant).sequence())
    }
    fn serialize_map(self, _: Option<usize>) -> Result<Self::SerializeMap> {
        Ok(self.object())
    }
    fn serialize_struct(self, _: &'static str, _: usize) -> Result<Self::SerializeStruct> {
        Ok(self.object())
    }
    fn serialize_struct_variant(
        self,
        _: &'static str,
        _: u32,
        variant: &'static str,
        _: usize,
    ) -> Result<Self::SerializeStructVariant> {
        Ok(self.variant(variant).object())
    }
}
struct Sequence<'a> {
    expected: Option<&'a Vec<Value>>,
    at: usize,
    matches: bool,
}
impl Sequence<'_> {
    fn element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        // Keep visiting after a mismatch, as serialization does.
        self.matches &=
            value.serialize(Compare(self.expected.and_then(|items| items.get(self.at))))?;
        self.at += 1;
        Ok(())
    }
    fn finish(self) -> Result<bool> {
        Ok(self.matches && self.expected.is_some_and(|items| items.len() == self.at))
    }
}
macro_rules! sequence {
    ($trait:ident, $method:ident) => {
        impl ser::$trait for Sequence<'_> {
            type Ok = bool;
            type Error = Error;
            fn $method<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
                self.element(value)
            }
            fn end(self) -> Result<bool> {
                self.finish()
            }
        }
    };
}
sequence!(SerializeSeq, serialize_element);
sequence!(SerializeTuple, serialize_element);
sequence!(SerializeTupleStruct, serialize_field);
sequence!(SerializeTupleVariant, serialize_field);

struct Object<'a> {
    expected: Option<&'a Map<String, Value>>,
    // Borrow expected keys. Updating by key preserves serde_json's duplicate
    // key semantics; no serialized key or child tree is retained.
    seen: BTreeMap<&'a str, bool>,
    pending: Option<Option<&'a str>>,
    matches: bool,
}
impl<'a> Object<'a> {
    fn key(&self, key: &str) -> Option<&'a str> {
        self.expected
            .and_then(|map| map.get_key_value(key))
            .map(|(key, _)| key.as_str())
    }
    fn value<T: Serialize + ?Sized>(&mut self, key: Option<&'a str>, value: &T) -> Result<()> {
        let expected = key.and_then(|key| self.expected.and_then(|map| map.get(key)));
        let matches = value.serialize(Compare(expected))?;
        if let Some(key) = key {
            self.seen.insert(key, matches);
        } else {
            self.matches = false;
        }
        Ok(())
    }
    fn finish(self) -> Result<bool> {
        Ok(self.matches
            && self
                .expected
                .is_some_and(|map| map.len() == self.seen.len())
            && self.seen.values().all(|matches| *matches))
    }
}
impl ser::SerializeMap for Object<'_> {
    type Ok = bool;
    type Error = Error;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<()> {
        let key = match serde_json::to_value(key)? {
            Value::String(key) => key,
            Value::Number(key) => key.to_string(),
            Value::Bool(key) => key.to_string(),
            _ => return Err(ser::Error::custom("JSON object keys must be strings")),
        };
        self.pending = Some(self.key(&key));
        Ok(())
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let key = self
            .pending
            .take()
            .ok_or_else(|| <Error as ser::Error>::custom("object value has no key"))?;
        self.value(key, value)
    }
    fn end(self) -> Result<bool> {
        self.finish()
    }
}
macro_rules! object {
    ($trait:ident) => {
        impl ser::$trait for Object<'_> {
            type Ok = bool;
            type Error = Error;
            fn serialize_field<T: Serialize + ?Sized>(
                &mut self,
                key: &'static str,
                value: &T,
            ) -> Result<()> {
                self.value(self.key(key), value)
            }
            fn end(self) -> Result<bool> {
                self.finish()
            }
        }
    };
}
object!(SerializeStruct);
object!(SerializeStructVariant);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[derive(Serialize)]
    enum Kind {
        Unit,
        New(u64),
        Tuple(bool, String),
        Record { a: f64 },
    }
    #[derive(Serialize)]
    struct Payload {
        bytes: Vec<u8>,
        kind: Kind,
        #[serde(skip_serializing_if = "Option::is_none")]
        optional: Option<bool>,
    }

    /// FIG-7354: spelling checks compare every emitted member, numeric kind,
    /// optional field and array position, preserving serde_json's value rules.
    #[test]
    fn typed_spelling_comparison_preserves_json_value_semantics() {
        for kind in [
            Kind::Unit,
            Kind::New(7),
            Kind::Tuple(true, "é\n".into()),
            Kind::Record { a: -0.0 },
        ] {
            let value = Payload {
                bytes: vec![0, 255],
                kind,
                optional: None,
            };
            let expected = serde_json::to_value(&value).unwrap();
            assert!(matches(&value, &expected));
            let mut changed = expected.clone();
            changed["optional"] = Value::Null;
            assert!(!matches(&value, &changed));
            changed = expected;
            changed["bytes"] = json!([255, 0]);
            assert!(!matches(&value, &changed));
        }
        assert!(!matches(&1u64, &json!(1.0)));
        assert!(matches(&f64::NAN, &Value::Null));
        assert!(matches(
            &BTreeMap::from([(17u64, true)]),
            &json!({"17":true})
        ));
        struct Duplicate;
        impl Serialize for Duplicate {
            fn serialize<S: ser::Serializer>(
                &self,
                serializer: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                use ser::SerializeMap as _;
                let mut map = serializer.serialize_map(Some(2))?;
                map.serialize_entry("key", &false)?;
                map.serialize_entry("key", &true)?;
                map.end()
            }
        }
        assert!(matches(&Duplicate, &json!({"key":true})));
        assert!(!matches(&Duplicate, &json!({"key":false})));
    }
}
