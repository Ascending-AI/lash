//! Length-only Serde traversal: never escape, copy, or scan string contents.
use serde::{Serialize, ser};

pub(super) struct RawBudget {
    remaining: usize,
}

impl RawBudget {
    pub(super) fn fits(value: &impl Serialize, remaining: usize) -> bool {
        value.serialize(&mut Self { remaining }).is_ok()
    }

    fn charge(&mut self, bytes: usize) -> Result<(), serde_json::Error> {
        self.remaining = self.remaining.checked_sub(bytes.max(1)).ok_or_else(|| {
            <serde_json::Error as ser::Error>::custom("request exceeds raw work budget")
        })?;
        Ok(())
    }
}

macro_rules! scalars {
    ($($method:ident($ty:ty)),* $(,)?) => {$(
        fn $method(self, _value: $ty) -> Result<(), Self::Error> { self.charge(1) }
    )*};
}

impl ser::Serializer for &mut RawBudget {
    type Ok = ();
    type Error = serde_json::Error;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    scalars! {
        serialize_bool(bool), serialize_i8(i8), serialize_i16(i16),
        serialize_i32(i32), serialize_i64(i64), serialize_i128(i128),
        serialize_u8(u8), serialize_u16(u16), serialize_u32(u32),
        serialize_u64(u64), serialize_u128(u128), serialize_f32(f32),
        serialize_f64(f64), serialize_char(char),
    }

    fn serialize_str(self, value: &str) -> Result<(), Self::Error> {
        self.charge(value.len())
    }
    fn serialize_bytes(self, value: &[u8]) -> Result<(), Self::Error> {
        self.charge(value.len())
    }
    fn serialize_none(self) -> Result<(), Self::Error> {
        self.charge(1)
    }
    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> Result<(), Self::Error> {
        self.charge(1)?;
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), Self::Error> {
        self.charge(1)
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), Self::Error> {
        self.charge(1)
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), Self::Error> {
        self.charge(variant.len())
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.charge(1)?;
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), Self::Error> {
        self.charge(variant.len())?;
        value.serialize(self)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self, Self::Error> {
        self.charge(len.unwrap_or(1))?;
        Ok(self)
    }
    fn serialize_tuple(self, len: usize) -> Result<Self, Self::Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_struct(self, _name: &'static str, len: usize) -> Result<Self, Self::Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self, Self::Error> {
        self.charge(variant.len())?;
        self.serialize_seq(Some(len))
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self, Self::Error> {
        self.serialize_seq(len)
    }
    fn serialize_struct(self, _name: &'static str, len: usize) -> Result<Self, Self::Error> {
        self.serialize_seq(Some(len))
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
        len: usize,
    ) -> Result<Self, Self::Error> {
        self.charge(variant.len())?;
        self.serialize_seq(Some(len))
    }
    // Unknown Display implementations may do unbounded work before emitting
    // anything. Conservatively offload rather than invoking one in the probe.
    fn collect_str<T: ?Sized + std::fmt::Display>(self, _value: &T) -> Result<(), Self::Error> {
        Err(<Self::Error as ser::Error>::custom(
            "display requires blocking work",
        ))
    }
}

macro_rules! sequences {
    ($($trait:ident::$method:ident),* $(,)?) => {$(
        impl ser::$trait for &mut RawBudget {
            type Ok = ();
            type Error = serde_json::Error;
            fn $method<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
                self.charge(1)?;
                value.serialize(&mut **self)
            }
            fn end(self) -> Result<(), Self::Error> { Ok(()) }
        }
    )*};
}
sequences! {
    SerializeSeq::serialize_element,
    SerializeTuple::serialize_element,
    SerializeTupleStruct::serialize_field,
    SerializeTupleVariant::serialize_field,
}

impl ser::SerializeMap for &mut RawBudget {
    type Ok = ();
    type Error = serde_json::Error;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> Result<(), Self::Error> {
        self.charge(1)?;
        key.serialize(&mut **self)
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> Result<(), Self::Error> {
        self.charge(1)?;
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), Self::Error> {
        Ok(())
    }
}

macro_rules! structs {
    ($($trait:ident),* $(,)?) => {$(
        impl ser::$trait for &mut RawBudget {
            type Ok = ();
            type Error = serde_json::Error;
            fn serialize_field<T: ?Sized + Serialize>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error> {
                self.charge(key.len())?;
                value.serialize(&mut **self)
            }
            fn end(self) -> Result<(), Self::Error> { Ok(()) }
        }
    )*};
}
structs! { SerializeStruct, SerializeStructVariant }
