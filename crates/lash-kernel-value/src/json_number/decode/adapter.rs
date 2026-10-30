use std::{cell::Cell, fmt, marker::PhantomData};

use serde::de::{
    self, Deserialize, DeserializeSeed, Deserializer, EnumAccess, MapAccess, SeqAccess,
    VariantAccess, Visitor,
};

use super::{NUMBER_TOKEN, VALUE_MARKER, decimal_text};
use crate::json_number::DECIMAL_EXT_CODE;

pub(super) struct Adapter<'a, D> {
    inner: D,
    refusal: &'a Cell<Option<String>>,
    number_target: bool,
}

impl<'a, D> Adapter<'a, D> {
    pub(super) fn new(inner: D, refusal: &'a Cell<Option<String>>) -> Self {
        Self {
            inner,
            refusal,
            number_target: false,
        }
    }
}

struct Wrapped<'a, V> {
    visitor: V,
    refusal: &'a Cell<Option<String>>,
    extension: bool,
    number_target: bool,
}

impl<'a, V> Wrapped<'a, V> {
    fn new(visitor: V, refusal: &'a Cell<Option<String>>, extension: bool) -> Self {
        Self {
            visitor,
            refusal,
            extension,
            number_target: false,
        }
    }
}

macro_rules! scalar_deserializers {
    ($($method:ident),* $(,)?) => {$(
        fn $method<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
            self.inner.$method(visitor)
        }
    )*};
}

impl<'de, D: Deserializer<'de>> Deserializer<'de> for Adapter<'_, D> {
    type Error = D::Error;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        let mut wrapped = Wrapped::new(visitor, self.refusal, true);
        wrapped.number_target = self.number_target;
        self.inner.deserialize_any(wrapped)
    }

    scalar_deserializers!(
        deserialize_bool,
        deserialize_i8,
        deserialize_i16,
        deserialize_i32,
        deserialize_i64,
        deserialize_i128,
        deserialize_u8,
        deserialize_u16,
        deserialize_u32,
        deserialize_u64,
        deserialize_u128,
        deserialize_f32,
        deserialize_f64,
        deserialize_char,
        deserialize_str,
        deserialize_string,
        deserialize_bytes,
        deserialize_byte_buf,
        deserialize_unit,
        deserialize_identifier,
        deserialize_ignored_any,
    );

    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.inner
            .deserialize_option(Wrapped::new(visitor, self.refusal, false))
    }

    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner.deserialize_unit_struct(name, visitor)
    }

    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        if name == VALUE_MARKER {
            visitor.visit_newtype_struct(Self {
                inner: self.inner,
                refusal: self.refusal,
                number_target: true,
            })
        } else {
            self.inner
                .deserialize_newtype_struct(name, Wrapped::new(visitor, self.refusal, false))
        }
    }

    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.inner
            .deserialize_seq(Wrapped::new(visitor, self.refusal, false))
    }

    fn deserialize_tuple<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner
            .deserialize_tuple(len, Wrapped::new(visitor, self.refusal, false))
    }

    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner
            .deserialize_tuple_struct(name, len, Wrapped::new(visitor, self.refusal, false))
    }

    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, Self::Error> {
        self.inner
            .deserialize_map(Wrapped::new(visitor, self.refusal, false))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner
            .deserialize_struct(name, fields, Wrapped::new(visitor, self.refusal, false))
    }

    fn deserialize_enum<V: Visitor<'de>>(
        self,
        name: &'static str,
        variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.inner
            .deserialize_enum(name, variants, Wrapped::new(visitor, self.refusal, false))
    }

    fn is_human_readable(&self) -> bool {
        self.inner.is_human_readable()
    }
}

macro_rules! visit_scalars {
    ($($method:ident($ty:ty)),* $(,)?) => {$(
        fn $method<E: de::Error>(self, value: $ty) -> Result<Self::Value, E> {
            self.visitor.$method(value)
        }
    )*};
}

impl<'de, V: Visitor<'de>> Visitor<'de> for Wrapped<'_, V> {
    type Value = V::Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.visitor.expecting(formatter)
    }

    visit_scalars!(
        visit_bool(bool),
        visit_i8(i8),
        visit_i16(i16),
        visit_i32(i32),
        visit_i64(i64),
        visit_i128(i128),
        visit_u8(u8),
        visit_u16(u16),
        visit_u32(u32),
        visit_u64(u64),
        visit_u128(u128),
        visit_f32(f32),
        visit_f64(f64),
        visit_char(char),
        visit_str(&str),
        visit_borrowed_str(&'de str),
        visit_string(String),
        visit_bytes(&[u8]),
        visit_borrowed_bytes(&'de [u8]),
        visit_byte_buf(Vec<u8>),
    );

    fn visit_none<E: de::Error>(self) -> Result<Self::Value, E> {
        self.visitor.visit_none()
    }

    fn visit_unit<E: de::Error>(self) -> Result<Self::Value, E> {
        self.visitor.visit_unit()
    }

    fn visit_some<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.visitor
            .visit_some(Adapter::new(deserializer, self.refusal))
    }

    fn visit_newtype_struct<D: Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<Self::Value, D::Error> {
        if !self.extension {
            return self
                .visitor
                .visit_newtype_struct(Adapter::new(deserializer, self.refusal));
        }
        let (code, bytes): (i8, &'de serde_bytes::Bytes) = Deserialize::deserialize(deserializer)?;
        if code == DECIMAL_EXT_CODE {
            let text = decimal_text::<D::Error>(bytes.as_ref(), self.refusal)?;
            let map = de::value::MapDeserializer::<_, D::Error>::new(std::iter::once((
                NUMBER_TOKEN,
                text,
            )));
            if self.number_target {
                self.visitor
                    .visit_newtype_struct(de::value::MapAccessDeserializer::new(map))
            } else {
                self.visitor.visit_map(map)
            }
        } else {
            self.visitor.visit_newtype_struct(ExtReplay::<D::Error> {
                code,
                bytes: bytes.as_ref(),
                error: PhantomData,
            })
        }
    }

    fn visit_seq<A: SeqAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
        self.visitor.visit_seq(Sequence {
            access,
            refusal: self.refusal,
        })
    }

    fn visit_map<A: MapAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
        self.visitor.visit_map(Mapping {
            access,
            refusal: self.refusal,
        })
    }

    fn visit_enum<A: EnumAccess<'de>>(self, access: A) -> Result<Self::Value, A::Error> {
        self.visitor.visit_enum(Enumeration {
            access,
            refusal: self.refusal,
        })
    }
}

struct Seed<'a, S> {
    seed: S,
    refusal: &'a Cell<Option<String>>,
}

impl<'de, S: DeserializeSeed<'de>> DeserializeSeed<'de> for Seed<'_, S> {
    type Value = S::Value;

    fn deserialize<D: Deserializer<'de>>(self, deserializer: D) -> Result<Self::Value, D::Error> {
        self.seed
            .deserialize(Adapter::new(deserializer, self.refusal))
    }
}

struct Sequence<'a, A> {
    access: A,
    refusal: &'a Cell<Option<String>>,
}

impl<'de, A: SeqAccess<'de>> SeqAccess<'de> for Sequence<'_, A> {
    type Error = A::Error;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.access.next_element_seed(Seed {
            seed,
            refusal: self.refusal,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        self.access.size_hint()
    }
}

struct Mapping<'a, A> {
    access: A,
    refusal: &'a Cell<Option<String>>,
}

impl<'de, A: MapAccess<'de>> MapAccess<'de> for Mapping<'_, A> {
    type Error = A::Error;

    fn next_key_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, Self::Error> {
        self.access.next_key_seed(Seed {
            seed,
            refusal: self.refusal,
        })
    }

    fn next_value_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<S::Value, Self::Error> {
        self.access.next_value_seed(Seed {
            seed,
            refusal: self.refusal,
        })
    }

    fn size_hint(&self) -> Option<usize> {
        self.access.size_hint()
    }
}

struct Enumeration<'a, A> {
    access: A,
    refusal: &'a Cell<Option<String>>,
}

impl<'de, 'a, A: EnumAccess<'de>> EnumAccess<'de> for Enumeration<'a, A> {
    type Error = A::Error;
    type Variant = Variant<'a, A::Variant>;

    fn variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<(S::Value, Self::Variant), Self::Error> {
        let (value, access) = self.access.variant_seed(Seed {
            seed,
            refusal: self.refusal,
        })?;
        Ok((
            value,
            Variant {
                access,
                refusal: self.refusal,
            },
        ))
    }
}

struct Variant<'a, A> {
    access: A,
    refusal: &'a Cell<Option<String>>,
}

impl<'de, A: VariantAccess<'de>> VariantAccess<'de> for Variant<'_, A> {
    type Error = A::Error;

    fn unit_variant(self) -> Result<(), Self::Error> {
        self.access.unit_variant()
    }

    fn newtype_variant_seed<S: DeserializeSeed<'de>>(
        self,
        seed: S,
    ) -> Result<S::Value, Self::Error> {
        self.access.newtype_variant_seed(Seed {
            seed,
            refusal: self.refusal,
        })
    }

    fn tuple_variant<V: Visitor<'de>>(
        self,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.access
            .tuple_variant(len, Wrapped::new(visitor, self.refusal, false))
    }

    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.access
            .struct_variant(fields, Wrapped::new(visitor, self.refusal, false))
    }
}

struct ExtReplay<'de, E> {
    code: i8,
    bytes: &'de [u8],
    error: PhantomData<E>,
}

impl<'de, E: de::Error> Deserializer<'de> for ExtReplay<'de, E> {
    type Error = E;

    fn deserialize_any<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, E> {
        visitor.visit_seq(ExtSequence {
            code: self.code,
            bytes: self.bytes,
            index: 0,
            error: PhantomData,
        })
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple tuple_struct
        map struct enum identifier ignored_any
    }
}

struct ExtSequence<'de, E> {
    code: i8,
    bytes: &'de [u8],
    index: u8,
    error: PhantomData<E>,
}

impl<'de, E: de::Error> SeqAccess<'de> for ExtSequence<'de, E> {
    type Error = E;

    fn next_element_seed<S: DeserializeSeed<'de>>(
        &mut self,
        seed: S,
    ) -> Result<Option<S::Value>, E> {
        let index = self.index;
        self.index = self.index.saturating_add(1);
        match index {
            0 => seed
                .deserialize(de::value::I8Deserializer::<E>::new(self.code))
                .map(Some),
            1 => seed
                .deserialize(de::value::BorrowedBytesDeserializer::<E>::new(self.bytes))
                .map(Some),
            _ => Ok(None),
        }
    }

    fn size_hint(&self) -> Option<usize> {
        Some(2_usize.saturating_sub(self.index as usize))
    }
}
