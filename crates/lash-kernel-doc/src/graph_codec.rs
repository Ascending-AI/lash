//! The canonical binary codecs of the kernel's vocabulary (FIG-5887 §2, §3).
//!
//! Every type here has one spelling in the token grammar of
//! [`crate::graph_tokens`], written and read by hand, never derived:
//!
//! - a struct is a sequence of exactly its fields in declaration order, every
//!   field present, empty or default ones included;
//! - an enum is a sum whose variant number is the type's
//!   [`CodecVariant`] table, with exactly that variant's field count;
//! - a `serde(transparent)` newtype (a name, an object or task handle, a
//!   timestamp) is its one field;
//! - an integer is always the arbitrary-integer token, a float always the
//!   binary64 token, a number token its exact lexical text;
//! - a content identity (a document or function id) is a 32-byte bytes
//!   token;
//! - a sorted map is the map token with strictly ascending keys, and a sorted
//!   set a sequence of strictly ascending members; insertion-ordered guest
//!   maps, sets and records are ordered sequences of their entries.
//!
//! Text and bytes are written whole. The graph layer places any token over
//! the inline bound in shared leaves, so equal text in a document, a request,
//! a result and a session binding is stored once.
//!
//! A decoder refuses every other spelling, and refuses nesting deeper than
//! [`MAX_DECODE_DEPTH`] before it can exhaust the stack.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use num_bigint::{BigInt, Sign};
use num_traits::ToPrimitive;

use crate::graph_tokens::{
    CodecVariant, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending,
};
use crate::{
    Bytes, ClosureObject, Datum, DocumentId, EffectIdentity, ErrorDatum, ErrorValue, Float,
    FunctionId, Handle, Identity, Integer, JoinMode, LoopIteration, Name, NumberPolicy,
    NumberToken, Object, ObjectId, QualifiedName, Site, SpawnIdentity, TaskId, TaskIdentity,
    Timestamp, Unit, Value,
};

/// The deepest a decoded value, form or type may nest, in recursive
/// decodes. Deeper input is refused as a [`DecodeDimension::Depth`] limit.
pub const MAX_DECODE_DEPTH: u64 = 512;

/// FrozenValue (node kind 8) variant numbers: what a frozen tree is.
pub const FROZEN_DATUM: u32 = 0;
pub const FROZEN_TOOL_VALUE: u32 = 1;
pub const FROZEN_JSON: u32 = 2;

/// A datum as the body of a FrozenValue node: `sum(0, 1, <Datum>)`. A
/// committed effect outcome and a run's result are staged through it, so
/// equal trees are one node.
#[derive(Clone, Debug, PartialEq)]
pub struct FrozenDatum(pub Datum);

impl TokenCodec for FrozenDatum {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(FROZEN_DATUM, 1);
        self.0.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        if variant != FROZEN_DATUM {
            return Err(unsupported(variant));
        }
        TokenReader::expect_fields(fields, 1)?;
        Datum::decode(input).map(Self)
    }
}

fn unsupported(variant: u32) -> TokenError {
    TokenError::UnsupportedVariant { variant }
}

fn invalid(problem: impl ToString) -> TokenError {
    TokenError::Invalid(problem.to_string())
}

/// Reads a sum header and checks the variant's field count with `arity`.
fn variant(
    input: &mut TokenReader<'_>,
    arity: impl FnOnce(u32) -> Option<u64>,
) -> Result<u32, TokenError> {
    let (variant, fields) = input.sum()?;
    let expected = arity(variant).ok_or_else(|| unsupported(variant))?;
    TokenReader::expect_fields(fields, expected)?;
    Ok(variant)
}

// Generic containers.

impl<T: TokenCodec> TokenCodec for Box<T> {
    fn encode(&self, out: &mut TokenWriter) {
        (**self).encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        T::decode(input).map(Box::new)
    }
}

impl<T: TokenCodec> TokenCodec for Arc<T> {
    fn encode(&self, out: &mut TokenWriter) {
        (**self).encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        T::decode(input).map(Arc::new)
    }
}

impl<T: TokenCodec> TokenCodec for Arc<[T]> {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(self.len() as u64);
        for item in self.iter() {
            item.encode(out);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Vec::<T>::decode(input).map(Arc::from)
    }
}

impl TokenCodec for Arc<str> {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.text().map(Arc::from)
    }
}

impl<A: TokenCodec, B: TokenCodec> TokenCodec for (A, B) {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.0.encode(out);
        self.1.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok((A::decode(input)?, B::decode(input)?))
    }
}

/// A sorted map: the map token, keys strictly ascending in their order.
impl<K: TokenCodec + Ord, V: TokenCodec> TokenCodec for BTreeMap<K, V> {
    fn encode(&self, out: &mut TokenWriter) {
        out.map(self.len() as u64);
        for (key, value) in self {
            key.encode(out);
            value.encode(out);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let count = input.map()?;
        let mut map = BTreeMap::new();
        for _ in 0..count {
            let key = K::decode(input)?;
            if map.last_key_value().is_some_and(|(last, _)| *last >= key) {
                return Err(crate::graph_tokens::CanonicalFault::UnsortedOrDuplicateKey.into());
            }
            let value = V::decode(input)?;
            map.insert(key, value);
        }
        Ok(map)
    }
}

/// A sorted set: a sequence of strictly ascending members.
impl<T: TokenCodec + Ord> TokenCodec for BTreeSet<T> {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(self.len() as u64);
        for item in self {
            item.encode(out);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let items = Vec::<T>::decode(input)?;
        check_ascending(&items)?;
        Ok(items.into_iter().collect())
    }
}

// Scalars and names.

impl TokenCodec for Integer {
    fn encode(&self, out: &mut TokenWriter) {
        match self.to_i64() {
            Some(small) => out.big_integer(small < 0, &small.unsigned_abs().to_le_bytes()),
            None => {
                let (sign, magnitude) = self.as_bigint().to_bytes_le();
                out.big_integer(sign == Sign::Minus, &magnitude);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (negative, magnitude) = input.big_integer()?;
        if magnitude.len() <= 8 {
            let mut bytes = [0_u8; 8];
            bytes[..magnitude.len()].copy_from_slice(magnitude);
            let unsigned = u64::from_le_bytes(bytes);
            let small = if negative {
                0_i64.checked_sub_unsigned(unsigned)
            } else {
                i64::try_from(unsigned).ok()
            };
            if let Some(small) = small {
                return Ok(Integer::from(small));
            }
        }
        let sign = if negative { Sign::Minus } else { Sign::Plus };
        Ok(Integer::new(BigInt::from_bytes_le(sign, magnitude)))
    }
}

impl TokenCodec for Float {
    fn encode(&self, out: &mut TokenWriter) {
        out.float(self.get());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.float().map(Float::new)
    }
}

impl TokenCodec for NumberToken {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.as_str());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        NumberToken::new(input.text()?).map_err(invalid)
    }
}

impl TokenCodec for Timestamp {
    fn encode(&self, out: &mut TokenWriter) {
        self.nanoseconds.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self {
            nanoseconds: Integer::decode(input)?,
        })
    }
}

impl TokenCodec for Bytes {
    fn encode(&self, out: &mut TokenWriter) {
        out.bytes(self.as_slice());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.bytes().map(Bytes::new)
    }
}

impl TokenCodec for Name {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.as_str());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.text().map(Name::new)
    }
}

impl TokenCodec for QualifiedName {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.as_str());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        QualifiedName::new(input.text()?).map_err(invalid)
    }
}

macro_rules! digest_codec {
    ($($name:ident),*) => {$(
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                out.bytes(self.as_bytes());
            }

            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                let bytes: [u8; 32] = input
                    .bytes()?
                    .try_into()
                    .map_err(|_| invalid(concat!(stringify!($name), " is not 32 bytes")))?;
                Ok(Self::from_bytes(bytes))
            }
        }
    )*};
}

digest_codec!(FunctionId, DocumentId);

macro_rules! handle_codec {
    ($($name:ident),*) => {$(
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                out.unsigned(self.0);
            }

            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                input.unsigned().map(Self)
            }
        }
    )*};
}

handle_codec!(ObjectId, TaskId);

macro_rules! unit_enum_codec {
    ($($name:ident { $($variant:ident = $number:literal),* $(,)? })*) => {$(
        impl TokenCodec for $name {
            fn encode(&self, out: &mut TokenWriter) {
                out.sum(self.codec_variant(), 0);
            }

            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                Ok(match variant(input, |_| Some(0))? {
                    $($number => Self::$variant,)*
                    other => return Err(unsupported(other)),
                })
            }
        }
    )*};
}

unit_enum_codec! {
    NumberPolicy { Float = 0, BySpelling = 1 }
    JoinMode { All = 0, AllSettled = 1, Race = 2, Any = 3 }
}

// Sites and identities.

impl TokenCodec for Unit {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Main => out.sum(self.codec_variant(), 0),
            Self::Function(name) => {
                out.sum(self.codec_variant(), 1);
                name.encode(out);
            }
            Self::Library(id) => {
                out.sum(self.codec_variant(), 1);
                id.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| match variant {
                0 => Some(0),
                1 | 2 => Some(1),
                _ => None,
            })? {
                0 => Self::Main,
                1 => Self::Function(Name::decode(input)?),
                _ => Self::Library(FunctionId::decode(input)?),
            },
        )
    }
}

impl TokenCodec for Site {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.unit.encode(out);
        self.path.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            unit: Unit::decode(input)?,
            path: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for TaskIdentity {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Main => out.sum(self.codec_variant(), 0),
            Self::Spawned(spawn) => {
                out.sum(self.codec_variant(), 1);
                out.seq(3);
                spawn.parent.encode(out);
                spawn.site.encode(out);
                out.unsigned(spawn.occurrence);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            Ok(
                match variant(input, |variant| match variant {
                    0 => Some(0),
                    1 => Some(1),
                    _ => None,
                })? {
                    0 => Self::Main,
                    _ => {
                        input.expect_struct(3)?;
                        Self::Spawned(SpawnIdentity {
                            parent: Arc::decode(input)?,
                            site: Site::decode(input)?,
                            occurrence: input.unsigned()?,
                        })
                    }
                },
            )
        })
    }
}

impl TokenCodec for LoopIteration {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.site.encode(out);
        out.unsigned(self.iteration);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            site: Site::decode(input)?,
            iteration: input.unsigned()?,
        })
    }
}

impl TokenCodec for EffectIdentity {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.task.encode(out);
        self.site.encode(out);
        out.unsigned(self.occurrence);
        self.loops.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            task: TaskIdentity::decode(input)?,
            site: Site::decode(input)?,
            occurrence: input.unsigned()?,
            loops: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for Identity {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::Object(id) => id.encode(out),
            Self::Task(id) => id.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 1).then_some(1))? {
                0 => Self::Object(ObjectId::decode(input)?),
                _ => Self::Task(TaskId::decode(input)?),
            },
        )
    }
}

// Values.

impl TokenCodec for Handle {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.kind);
        out.text(&self.id);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            kind: String::decode(input)?,
            id: String::decode(input)?,
        })
    }
}

impl TokenCodec for ErrorValue {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        out.text(&self.kind);
        out.text(&self.message);
        self.data.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            kind: String::decode(input)?,
            message: String::decode(input)?,
            data: Value::decode(input)?,
        })
    }
}

impl TokenCodec for Value {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Null | Self::Absent => out.sum(number, 0),
            Self::Bool(value) => {
                out.sum(number, 1);
                out.bool(*value);
            }
            Self::Int(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Float(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Text(value) => {
                out.sum(number, 1);
                out.text(value);
            }
            Self::Bytes(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Timestamp(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Tuple(members) => {
                out.sum(number, 1);
                members.encode(out);
            }
            Self::List(id)
            | Self::Map(id)
            | Self::Set(id)
            | Self::Record(id)
            | Self::Closure(id) => {
                out.sum(number, 1);
                id.encode(out);
            }
            Self::Error(error) => {
                out.sum(number, 1);
                error.encode(out);
            }
            Self::Task(id) => {
                out.sum(number, 1);
                id.encode(out);
            }
            Self::Function(name) => {
                out.sum(number, 1);
                name.encode(out);
            }
            Self::Handle(handle) => {
                out.sum(number, 1);
                handle.encode(out);
            }
            Self::Ref(identity) => {
                out.sum(number, 1);
                identity.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0 | 1 => Some(0),
                2..=18 => Some(1),
                _ => None,
            })?;
            Ok(match number {
                0 => Self::Null,
                1 => Self::Absent,
                2 => Self::Bool(input.bool()?),
                3 => Self::Int(Integer::decode(input)?),
                4 => Self::Float(Float::decode(input)?),
                5 => Self::Text(Arc::decode(input)?),
                6 => Self::Bytes(Bytes::decode(input)?),
                7 => Self::Timestamp(Timestamp::decode(input)?),
                8 => Self::Tuple(Arc::decode(input)?),
                9 => Self::List(ObjectId::decode(input)?),
                10 => Self::Map(ObjectId::decode(input)?),
                11 => Self::Set(ObjectId::decode(input)?),
                12 => Self::Record(ObjectId::decode(input)?),
                13 => Self::Closure(ObjectId::decode(input)?),
                14 => Self::Error(Arc::decode(input)?),
                15 => Self::Task(TaskId::decode(input)?),
                16 => Self::Function(Name::decode(input)?),
                17 => Self::Handle(Arc::decode(input)?),
                _ => Self::Ref(Identity::decode(input)?),
            })
        })
    }
}

impl TokenCodec for ClosureObject {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.site.encode(out);
        self.captures.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            site: Site::decode(input)?,
            captures: Vec::decode(input)?,
        })
    }
}

impl TokenCodec for Object {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(self.codec_variant(), 1);
        match self {
            Self::List(items) | Self::Set(items) => items.encode(out),
            Self::Map(entries) => entries.encode(out),
            Self::Record(fields) => fields.encode(out),
            Self::Closure(closure) => closure.encode(out),
            Self::Variable(value) => value.encode(out),
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(
            match variant(input, |variant| (variant <= 5).then_some(1))? {
                0 => Self::List(Vec::decode(input)?),
                1 => Self::Map(Vec::decode(input)?),
                2 => Self::Set(Vec::decode(input)?),
                3 => Self::Record(Vec::decode(input)?),
                4 => Self::Closure(ClosureObject::decode(input)?),
                _ => Self::Variable(Value::decode(input)?),
            },
        )
    }
}

impl TokenCodec for ErrorDatum {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        out.text(&self.kind);
        out.text(&self.message);
        self.data.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            kind: String::decode(input)?,
            message: String::decode(input)?,
            data: Datum::decode(input)?,
        })
    }
}

impl TokenCodec for Datum {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Null | Self::Absent => out.sum(number, 0),
            Self::Bool(value) => {
                out.sum(number, 1);
                out.bool(*value);
            }
            Self::Int(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Float(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Number(token) => {
                out.sum(number, 1);
                token.encode(out);
            }
            Self::Text(value) => {
                out.sum(number, 1);
                out.text(value);
            }
            Self::Bytes(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Timestamp(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
            Self::Tuple(items) | Self::List(items) | Self::Set(items) => {
                out.sum(number, 1);
                items.encode(out);
            }
            Self::Map(entries) => {
                out.sum(number, 1);
                entries.encode(out);
            }
            Self::Record(fields) => {
                out.sum(number, 1);
                fields.encode(out);
            }
            Self::Error(error) => {
                out.sum(number, 1);
                error.encode(out);
            }
            Self::Function(name) => {
                out.sum(number, 1);
                name.encode(out);
            }
            Self::Handle(handle) => {
                out.sum(number, 1);
                handle.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0 | 1 => Some(0),
                2..=16 => Some(1),
                _ => None,
            })?;
            Ok(match number {
                0 => Self::Null,
                1 => Self::Absent,
                2 => Self::Bool(input.bool()?),
                3 => Self::Int(Integer::decode(input)?),
                4 => Self::Float(Float::decode(input)?),
                5 => Self::Number(NumberToken::decode(input)?),
                6 => Self::Text(String::decode(input)?),
                7 => Self::Bytes(Bytes::decode(input)?),
                8 => Self::Timestamp(Timestamp::decode(input)?),
                9 => Self::Tuple(Vec::decode(input)?),
                10 => Self::List(Vec::decode(input)?),
                11 => Self::Map(Vec::decode(input)?),
                12 => Self::Set(Vec::decode(input)?),
                13 => Self::Record(Vec::decode(input)?),
                14 => Self::Error(Box::decode(input)?),
                15 => Self::Function(Name::decode(input)?),
                _ => Self::Handle(Handle::decode(input)?),
            })
        })
    }
}

#[path = "graph_codec_forms.rs"]
mod forms;

#[cfg(test)]
#[path = "graph_codec_tests.rs"]
mod tests;
