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
    Bytes, Datum, DocumentId, EffectIdentity, ErrorDatum, Float, FunctionId, Handle, Integer,
    JoinMode, LoopIteration, Name, NumberPolicy, NumberToken, QualifiedName, Site, SpawnIdentity,
    TaskId, TaskIdentity, Timestamp, Unit,
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

handle_codec!(TaskId);

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
            Self::FunctionValue(value) => {
                out.sum(number, 1);
                value.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0 | 1 => Some(0),
                2..=17 => Some(1),
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
                16 => Self::Handle(Handle::decode(input)?),
                _ => Self::FunctionValue(Box::decode(input)?),
            })
        })
    }
}

#[path = "graph_codec_forms.rs"]
mod forms;

pub(crate) use forms::decode_header;

#[cfg(test)]
#[path = "graph_codec_tests.rs"]
mod tests;

/// Canonical codecs for semantic heap descriptors. Grants are activation
/// authority and are deliberately absent from the durable grammar.
pub mod heap_values {
    use super::{invalid, variant};
    use crate::graph_tokens::{ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter};
    use crate::heap::*;
    use crate::{Float, Integer, TaskId, Timestamp};
    use num_bigint::{BigInt, Sign};
    use num_traits::FromPrimitive;
    use sha2::{Digest, Sha256};
    use std::sync::Arc;

    macro_rules! record {
        ($name:ident { $($field:ident : $ty:ty),+ $(,)? }) => {
            impl TokenCodec for $name {
                fn encode(&self, out: &mut TokenWriter) {
                    out.seq([$(stringify!($field)),+].len() as u64);
                    $(self.$field.encode(out);)+
                }
                fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                    input.expect_struct([$(stringify!($field)),+].len() as u64)?;
                    Ok(Self { $($field: <$ty>::decode(input)?),+ })
                }
            }
        };
    }
    macro_rules! scalar {
        ($name:ident) => {
            impl TokenCodec for $name {
                fn encode(&self, out: &mut TokenWriter) {
                    out.unsigned(self.0);
                }
                fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                    input.unsigned().map(Self)
                }
            }
        };
    }
    scalar!(FamilyId);
    scalar!(LogicalBytes);
    record!(EntityId {
        family: FamilyId,
        ordinal: u64
    });
    record!(TextSummary {
        utf8_bytes: u64,
        chars: u64,
        utf16_units: u64
    });
    record!(IntSummary {
        bits: u64,
        negative: bool
    });
    record!(TupleSummary {
        len: u64,
        task_edges: u64,
        cost: LogicalBytes,
        has_edges: bool,
        depth: u32
    });

    impl TokenCodec for PageId {
        fn encode(&self, out: &mut TokenWriter) {
            out.child(ChildRef(self.0));
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.child().map(|id| Self(id.0))
        }
    }
    impl TokenCodec for PageRef {
        fn encode(&self, out: &mut TokenWriter) {
            self.id.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            Ok(Self {
                id: PageId::decode(input)?,
                grant: PageGrant::NONE,
            })
        }
    }
    impl TokenCodec for Identity {
        fn encode(&self, out: &mut TokenWriter) {
            match self {
                Self::Object(id) => {
                    out.sum(0, 1);
                    id.encode(out);
                }
                Self::Task(id) => {
                    out.sum(1, 1);
                    id.encode(out);
                }
            }
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            Ok(match variant(input, |v| (v <= 1).then_some(1))? {
                0 => Self::Object(EntityId::decode(input)?),
                _ => Self::Task(TaskId::decode(input)?),
            })
        }
    }
    impl TokenCodec for TextRef {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(2);
            self.summary.encode(out);
            match &self.body {
                Payload::Resident(text) => {
                    out.sum(0, 1);
                    out.text(text);
                }
                Payload::Paged(page) => {
                    out.sum(1, 1);
                    page.encode(out);
                }
            }
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.expect_struct(2)?;
            let summary = TextSummary::decode(input)?;
            let body = match variant(input, |v| (v <= 1).then_some(1))? {
                0 => {
                    let text = input.text()?;
                    if text.len() as u64 > IMMUTABLE_INLINE_MAX || summary != text_summary(text) {
                        return Err(invalid("invalid inline text descriptor"));
                    }
                    Payload::Resident(Arc::from(text))
                }
                _ => {
                    if summary.utf8_bytes <= IMMUTABLE_INLINE_MAX {
                        return Err(invalid("a short text must be inline"));
                    }
                    Payload::paged(PageRef::decode(input)?)
                }
            };
            Ok(Self {
                summary,
                body,
                normal: Default::default(),
            })
        }
    }
    impl TokenCodec for BytesRef {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(2);
            out.unsigned(self.len);
            match &self.body {
                Payload::Resident(bytes) => {
                    out.sum(0, 1);
                    out.bytes(bytes);
                }
                Payload::Paged(page) => {
                    out.sum(1, 1);
                    page.encode(out);
                }
            }
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            input.expect_struct(2)?;
            let len = input.unsigned()?;
            let body = match variant(input, |v| (v <= 1).then_some(1))? {
                0 => {
                    let bytes = input.bytes()?;
                    if bytes.len() as u64 != len || len > IMMUTABLE_INLINE_MAX {
                        return Err(invalid("invalid inline bytes descriptor"));
                    }
                    Payload::Resident(Arc::from(bytes))
                }
                _ => {
                    if len <= IMMUTABLE_INLINE_MAX {
                        return Err(invalid("short bytes must be inline"));
                    }
                    Payload::paged(PageRef::decode(input)?)
                }
            };
            Ok(Self {
                len,
                body,
                normal: Default::default(),
            })
        }
    }
    impl TokenCodec for IntRef {
        fn encode(&self, out: &mut TokenWriter) {
            match self {
                Self::Inline(int) => {
                    out.sum(0, 1);
                    int.encode(out);
                }
                Self::Paged {
                    summary, digits, ..
                } => {
                    out.sum(1, 2);
                    summary.encode(out);
                    digits.encode(out);
                }
            }
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            Ok(
                match variant(input, |v| match v {
                    0 => Some(1),
                    1 => Some(2),
                    _ => None,
                })? {
                    0 => {
                        let int = Integer::decode(input)?;
                        if int.bits().div_ceil(8) > IMMUTABLE_INLINE_MAX {
                            return Err(invalid("a large integer must be paged"));
                        }
                        Self::Inline(int)
                    }
                    _ => {
                        let summary = IntSummary::decode(input)?;
                        if summary.bits.div_ceil(8) <= IMMUTABLE_INLINE_MAX {
                            return Err(invalid("a short integer must be inline"));
                        }
                        Self::Paged {
                            normal: Default::default(),
                            summary,
                            digits: Arc::new(PageRef::decode(input)?),
                        }
                    }
                },
            )
        }
    }
    impl TokenCodec for Payload<PageValue> {
        fn encode(&self, out: &mut TokenWriter) {
            match self {
                Self::Resident(v) => {
                    out.sum(0, 1);
                    v.encode(out);
                }
                Self::Paged(p) => {
                    out.sum(1, 1);
                    p.encode(out);
                }
            }
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            Ok(match variant(input, |v| (v <= 1).then_some(1))? {
                0 => Self::Resident(Arc::new(PageValue::decode(input)?)),
                _ => Self::paged(PageRef::decode(input)?),
            })
        }
    }
    impl TokenCodec for ErrorRef {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(8);
            self.kind.encode(out);
            self.message.encode(out);
            self.data.encode(out);
            self.depth.encode(out);
            self.cost.encode(out);
            self.has_edges.encode(out);
            self.family.encode(out);
            self.task_edges.encode(out);
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            decode_error(input, None)
        }
    }
    fn decode_error(
        input: &mut TokenReader<'_>,
        family: Option<FamilyId>,
    ) -> Result<ErrorRef, TokenError> {
        input.expect_struct(8)?;
        let kind = TextRef::decode(input)?;
        let message = TextRef::decode(input)?;
        let data = match variant(input, |v| (v <= 1).then_some(1))? {
            0 => Payload::Resident(Arc::new(decode_value_with_family(input, family)?)),
            _ => Payload::paged(PageRef::decode(input)?),
        };
        let depth = u32::decode(input)?;
        if depth == 0 || depth > 128 {
            return Err(invalid("error depth exceeds K-VAL-034"));
        }
        let cost = LogicalBytes::decode(input)?;
        let has_edges = bool::decode(input)?;
        let declared = Option::<FamilyId>::decode(input)?;
        if family.is_some() && declared.is_some() && family != declared {
            return Err(invalid("error family context mismatch"));
        }
        let family = declared.or(family);
        let task_edges = u64::decode(input)?;
        if let Payload::Resident(value) = &data {
            if task_count(value) != task_edges
                || nesting(value) + 1 != depth
                || contains_edges(value) != has_edges
            {
                return Err(invalid("error edge/depth summary mismatch"));
            }
            let expected = kind
                .summary
                .utf8_bytes
                .checked_add(message.summary.utf8_bytes)
                .and_then(|v| v.checked_add(local_cost(value)))
                .ok_or_else(|| invalid("error cost overflow"))?;
            if expected != cost.0 {
                return Err(invalid("error cost summary mismatch"));
            }
        }
        Ok(ErrorRef {
            kind,
            message,
            data,
            depth,
            cost,
            has_edges,
            family,
            task_edges,
        })
    }
    impl TokenCodec for TupleRef {
        fn encode(&self, out: &mut TokenWriter) {
            out.seq(3);
            self.summary.encode(out);
            self.family.encode(out);
            match &self.body {
                Payload::Resident(values) => {
                    out.sum(0, 1);
                    values.encode(out);
                }
                Payload::Paged(page) => {
                    out.sum(1, 1);
                    page.encode(out);
                }
            }
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            decode_tuple(input, None)
        }
    }
    fn decode_tuple(
        input: &mut TokenReader<'_>,
        inherited: Option<FamilyId>,
    ) -> Result<TupleRef, TokenError> {
        input.expect_struct(3)?;
        let summary = TupleSummary::decode(input)?;
        let declared = Option::<FamilyId>::decode(input)?;
        if inherited.is_some() && declared.is_some() && inherited != declared {
            return Err(invalid("tuple family context mismatch"));
        }
        let family = declared.or(inherited);
        if summary.depth > 128 {
            return Err(invalid("tuple depth exceeds K-VAL-034"));
        }
        let body = match variant(input, |v| (v <= 1).then_some(1))? {
            0 => {
                let count = input.seq()?;
                if count != summary.len {
                    return Err(invalid("tuple length mismatch"));
                }
                let mut values = Vec::new();
                for _ in 0..count {
                    values.push(decode_value_with_family(input, family)?);
                }
                let count = values.iter().try_fold(0u64, |n, v| {
                    n.checked_add(task_count(v))
                        .ok_or_else(|| invalid("tuple task-edge overflow"))
                })?;
                let cost = values.iter().try_fold(0u64, |n, v| {
                    n.checked_add(local_cost(v))
                        .ok_or_else(|| invalid("tuple cost overflow"))
                })?;
                let depth = values.iter().map(nesting).max().unwrap_or(0) + 1;
                if count != summary.task_edges
                    || cost != summary.cost.0
                    || depth != summary.depth
                    || values.iter().any(contains_edges) != summary.has_edges
                {
                    return Err(invalid("tuple aggregate mismatch"));
                }
                Payload::Resident(Arc::from(values))
            }
            _ => Payload::paged(PageRef::decode(input)?),
        };
        Ok(TupleRef {
            normal: Default::default(),
            summary,
            family,
            body,
        })
    }
    impl TokenCodec for PageValue {
        fn encode(&self, out: &mut TokenWriter) {
            macro_rules! one {
                ($n:expr, $v:expr) => {{
                    out.sum($n, 1);
                    $v.encode(out);
                }};
            }
            match self {
                Self::Null => out.sum(0, 0),
                Self::Absent => out.sum(1, 0),
                Self::Bool(v) => one!(2, v),
                Self::Int(v) => one!(3, v),
                Self::Float(v) => one!(4, v),
                Self::Text(v) => one!(5, v),
                Self::Bytes(v) => one!(6, v),
                Self::Timestamp(v) => one!(7, v),
                Self::Tuple(v) => one!(8, v),
                Self::List(v) => one!(9, v),
                Self::Map(v) => one!(10, v),
                Self::Set(v) => one!(11, v),
                Self::Record(v) => one!(12, v),
                Self::Closure(v) => one!(13, v),
                Self::Error(v) => one!(14, v),
                Self::Task(v) => one!(15, v),
                Self::Function(v) => one!(16, v),
                Self::Handle(v) => one!(17, v),
                Self::Ref(v) => one!(18, v),
            }
        }
        fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
            decode_value_with_family(input, None)
        }
    }
    /// A relative occurrence has a distinct grammar tag. It cannot be decoded
    /// without a family supplied by the frozen body's authenticated link.
    pub fn encode_relative(kind: u32, ordinal: u64, out: &mut TokenWriter) {
        out.sum(19, 2);
        out.unsigned(u64::from(kind));
        out.unsigned(ordinal);
    }
    fn absolute_entity(
        input: &mut TokenReader<'_>,
        family: Option<FamilyId>,
    ) -> Result<EntityId, TokenError> {
        let id = EntityId::decode(input)?;
        if Some(id.family) == family {
            return Err(invalid("absolute reference under its own family context"));
        }
        Ok(id)
    }
    pub fn decode_value_with_family(
        input: &mut TokenReader<'_>,
        family: Option<FamilyId>,
    ) -> Result<PageValue, TokenError> {
        input.nested(|input| {
            Ok(
                match variant(input, |v| match v {
                    0 | 1 => Some(0),
                    2..=18 => Some(1),
                    19 => Some(2),
                    _ => None,
                })? {
                    0 => PageValue::Null,
                    1 => PageValue::Absent,
                    2 => PageValue::Bool(bool::decode(input)?),
                    3 => PageValue::Int(IntRef::decode(input)?),
                    4 => PageValue::Float(Float::decode(input)?),
                    5 => PageValue::Text(TextRef::decode(input)?),
                    6 => PageValue::Bytes(BytesRef::decode(input)?),
                    7 => PageValue::Timestamp(Timestamp::decode(input)?),
                    8 => PageValue::Tuple(Arc::new(decode_tuple(input, family)?)),
                    9 => PageValue::List(absolute_entity(input, family)?),
                    10 => PageValue::Map(absolute_entity(input, family)?),
                    11 => PageValue::Set(absolute_entity(input, family)?),
                    12 => PageValue::Record(absolute_entity(input, family)?),
                    13 => PageValue::Closure(absolute_entity(input, family)?),
                    14 => PageValue::Error(Arc::new(decode_error(input, family)?)),
                    15 => PageValue::Task(TaskId::decode(input)?),
                    16 => PageValue::Function(TextRef::decode(input)?),
                    17 => PageValue::Handle(Arc::decode(input)?),
                    18 => {
                        let identity = Identity::decode(input)?;
                        if matches!(identity, Identity::Object(id) if Some(id.family) == family) {
                            return Err(invalid("absolute reference under its own family context"));
                        }
                        PageValue::Ref(identity)
                    }
                    _ => {
                        let kind = input.unsigned_u32()?;
                        let ordinal = input.unsigned()?;
                        let id = EntityId {
                            family: family.ok_or_else(|| {
                                invalid("relative reference without family context")
                            })?,
                            ordinal,
                        };
                        match kind {
                            0 => PageValue::List(id),
                            1 => PageValue::Map(id),
                            2 => PageValue::Set(id),
                            3 => PageValue::Record(id),
                            4 => PageValue::Closure(id),
                            5 => PageValue::Ref(Identity::Object(id)),
                            _ => return Err(invalid("unknown relative reference kind")),
                        }
                    }
                },
            )
        })
    }

    /// LocalCostV1 prices immutable data at each holding location.
    pub fn local_cost(value: &PageValue) -> u64 {
        16u64.saturating_add(match value {
            PageValue::Int(IntRef::Inline(v)) => v.bits().div_ceil(8),
            PageValue::Int(IntRef::Paged { summary, .. }) => summary.bits.div_ceil(8),
            PageValue::Text(v) => v.summary.utf8_bytes,
            PageValue::Bytes(v) => v.len,
            PageValue::Timestamp(v) => v.nanoseconds.bits().div_ceil(8),
            PageValue::Tuple(v) => v.summary.cost.0,
            PageValue::Error(v) => v.cost.0,
            PageValue::Function(v) => v.summary.utf8_bytes,
            PageValue::Handle(v) => (v.kind.len() + v.id.len()) as u64,
            _ => 0,
        })
    }
    fn nesting(value: &PageValue) -> u32 {
        match value {
            PageValue::Tuple(t) => t.summary.depth,
            PageValue::Error(e) => e.depth,
            _ => 0,
        }
    }
    fn contains_edges(value: &PageValue) -> bool {
        matches!(
            value,
            PageValue::List(_)
                | PageValue::Map(_)
                | PageValue::Set(_)
                | PageValue::Record(_)
                | PageValue::Closure(_)
                | PageValue::Task(_)
                | PageValue::Ref(_)
        ) || matches!(value, PageValue::Tuple(t) if t.summary.has_edges)
            || matches!(value, PageValue::Error(e) if e.has_edges)
    }
    pub fn task_count(value: &PageValue) -> u64 {
        match value {
            PageValue::Task(_) | PageValue::Ref(Identity::Task(_)) => 1,
            PageValue::Tuple(t) => t.summary.task_edges,
            PageValue::Error(e) => e.task_edges,
            _ => 0,
        }
    }
    pub fn text_summary(text: &str) -> TextSummary {
        if text.is_ascii() {
            let len = text.len() as u64;
            return TextSummary {
                utf8_bytes: len,
                chars: len,
                utf16_units: len,
            };
        }
        TextSummary {
            utf8_bytes: text.len() as u64,
            chars: text.chars().count() as u64,
            utf16_units: text.encode_utf16().count() as u64,
        }
    }
    /// Canonical long-key digest, distinct from page hashing (D-KEY).
    pub fn key_digest(tokens: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(b"lash.normal-key.v1\0");
        hash.update(tokens);
        hash.finalize().into()
    }
    pub fn integral_float(float: f64) -> Option<Integer> {
        (float.is_finite() && float.fract() == 0.0)
            .then(|| BigInt::from_f64(float).map(Integer::new))
            .flatten()
    }
    pub fn integer_digits(int: &Integer) -> Vec<u8> {
        int.as_bigint().to_bytes_le().1
    }
    pub fn integer_from_digits(negative: bool, digits: &[u8]) -> Integer {
        Integer::new(BigInt::from_bytes_le(
            if negative { Sign::Minus } else { Sign::Plus },
            digits,
        ))
    }
}
