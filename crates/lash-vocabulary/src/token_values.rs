//! Canonical encodings of neutral leaves, moved without changing bytes.
use crate::names::*;
use crate::operations::*;
use crate::tokens::*;
use crate::types::*;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};
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

impl TokenCodec for Name {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.as_str());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.text().map(Name::new)
    }
}

impl TokenCodec for FunctionName {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.as_str());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        FunctionName::new(input.text()?).map_err(invalid)
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

// Types and signatures.

impl TokenCodec for Type {
    fn encode(&self, out: &mut TokenWriter) {
        let number = self.codec_variant();
        match self {
            Self::Any
            | Self::Null
            | Self::Absent
            | Self::Bool
            | Self::Int
            | Self::Float
            | Self::Number
            | Self::Text
            | Self::Bytes
            | Self::Timestamp
            | Self::Error
            | Self::Json => out.sum(number, 0),
            Self::Tuple(types) | Self::Union(types) => {
                out.sum(number, 1);
                types.encode(out);
            }
            Self::List(inner) | Self::Set(inner) | Self::Task(inner) => {
                out.sum(number, 1);
                inner.encode(out);
            }
            Self::Map(map) => {
                out.sum(number, 1);
                map.encode(out);
            }
            Self::Record(record) => {
                out.sum(number, 1);
                record.encode(out);
            }
            Self::Enum(texts) => {
                out.sum(number, 1);
                texts.encode(out);
            }
            Self::Function(signature) => {
                out.sum(number, 1);
                signature.encode(out);
            }
            Self::Handle(kind) => {
                out.sum(number, 1);
                out.text(kind);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0..=9 | 18 | 21 => Some(0),
                10..=17 | 19 | 20 => Some(1),
                _ => None,
            })?;
            Ok(match number {
                0 => Self::Any,
                1 => Self::Null,
                2 => Self::Absent,
                3 => Self::Bool,
                4 => Self::Int,
                5 => Self::Float,
                6 => Self::Number,
                7 => Self::Text,
                8 => Self::Bytes,
                9 => Self::Timestamp,
                10 => Self::Tuple(Vec::decode(input)?),
                11 => Self::List(Box::decode(input)?),
                12 => Self::Map(Box::decode(input)?),
                13 => Self::Set(Box::decode(input)?),
                14 => Self::Record(RecordType::decode(input)?),
                15 => Self::Enum(Vec::decode(input)?),
                16 => Self::Function(Box::decode(input)?),
                17 => Self::Task(Box::decode(input)?),
                18 => Self::Error,
                19 => Self::Handle(String::decode(input)?),
                20 => Self::Union(Vec::decode(input)?),
                _ => Self::Json,
            })
        })
    }
}

impl TokenCodec for MapType {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.key.encode(out);
        self.value.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            key: Type::decode(input)?,
            value: Type::decode(input)?,
        })
    }
}

impl TokenCodec for RecordType {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.fields.encode(out);
        self.rest.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            fields: Vec::decode(input)?,
            rest: RecordRest::decode(input)?,
        })
    }
}

impl TokenCodec for RecordRest {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Closed => out.sum(0, 0),
            Self::Drop => out.sum(1, 0),
            Self::Keep(ty) => {
                out.sum(2, 1);
                ty.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            Ok(
                match variant(input, |v| match v {
                    0 | 1 => Some(0),
                    2 => Some(1),
                    _ => None,
                })? {
                    0 => Self::Closed,
                    1 => Self::Drop,
                    _ => Self::Keep(Box::decode(input)?),
                },
            )
        })
    }
}

impl TokenCodec for RecordTypeField {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        out.text(&self.name);
        self.ty.encode(out);
        out.bool(self.optional);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            name: String::decode(input)?,
            ty: Type::decode(input)?,
            optional: input.bool()?,
        })
    }
}

impl TokenCodec for Param {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.name.encode(out);
        self.ty.encode(out);
        out.bool(self.optional);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        Ok(Self {
            name: Name::decode(input)?,
            ty: Type::decode(input)?,
            optional: input.bool()?,
        })
    }
}

impl TokenCodec for Signature {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.params.encode(out);
        self.result.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            params: Vec::decode(input)?,
            result: Type::decode(input)?,
        })
    }
}

impl TokenCodec for ContractId {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.as_str());
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(Self::new(String::decode(input)?))
    }
}

impl TokenCodec for OperationRequirement {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.signature.encode(out);
        self.contract.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            signature: Signature::decode(input)?,
            contract: ContractId::decode(input)?,
        })
    }
}

impl TokenCodec for serde_json::Value {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Null => out.sum(0, 0),
            Self::Bool(value) => {
                out.sum(1, 1);
                out.bool(*value);
            }
            Self::Number(number) => {
                out.sum(2, 1);
                if let Some(unsigned) = number.as_u64() {
                    out.sum(0, 1);
                    out.unsigned(unsigned);
                } else if let Some(signed) = number.as_i64() {
                    out.sum(1, 1);
                    out.signed(signed);
                } else {
                    out.sum(2, 1);
                    out.float(number.as_f64().unwrap_or(f64::NAN));
                }
            }
            Self::String(text) => {
                out.sum(3, 1);
                out.text(text);
            }
            Self::Array(items) => {
                out.sum(4, 1);
                items.encode(out);
            }
            Self::Object(entries) => {
                out.sum(5, 1);
                let mut sorted: Vec<(&String, &serde_json::Value)> = entries.iter().collect();
                sorted.sort_by(|left, right| left.0.as_bytes().cmp(right.0.as_bytes()));
                out.map(sorted.len() as u64);
                for (key, value) in sorted {
                    out.text(key);
                    value.encode(out);
                }
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.nested(|input| {
            let number = variant(input, |variant| match variant {
                0 => Some(0),
                1..=5 => Some(1),
                _ => None,
            })?;
            Ok(match number {
                0 => Self::Null,
                1 => Self::Bool(input.bool()?),
                2 => Self::Number(
                    match variant(input, |variant| (variant <= 2).then_some(1))? {
                        0 => serde_json::Number::from(input.unsigned()?),
                        1 => {
                            let signed = input.signed()?;
                            if signed >= 0 {
                                return Err(invalid("a negative JSON integer is not negative"));
                            }
                            serde_json::Number::from(signed)
                        }
                        _ => {
                            let float = input.float()?;
                            let number = serde_json::Number::from_f64(float)
                                .ok_or_else(|| invalid("a JSON number is not finite"))?;
                            if number.as_u64().is_some() || number.as_i64().is_some() {
                                return Err(invalid("a JSON float spells an integer"));
                            }
                            number
                        }
                    },
                ),
                3 => Self::String(String::decode(input)?),
                4 => Self::Array(Vec::decode(input)?),
                _ => {
                    let count = input.map()?;
                    let mut entries = serde_json::Map::new();
                    let mut last: Option<String> = None;
                    for _ in 0..count {
                        let key = String::decode(input)?;
                        if last
                            .as_ref()
                            .is_some_and(|last| last.as_bytes() >= key.as_bytes())
                        {
                            return Err(CanonicalFault::UnsortedOrDuplicateKey.into());
                        }
                        let value = serde_json::Value::decode(input)?;
                        last = Some(key.clone());
                        entries.insert(key, value);
                    }
                    Self::Object(entries)
                }
            })
        })
    }
}

// Library-function definitions.

impl CodecVariant for Type {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Any => 0,
            Self::Null => 1,
            Self::Absent => 2,
            Self::Bool => 3,
            Self::Int => 4,
            Self::Float => 5,
            Self::Number => 6,
            Self::Text => 7,
            Self::Bytes => 8,
            Self::Timestamp => 9,
            Self::Tuple(..) => 10,
            Self::List(..) => 11,
            Self::Map(..) => 12,
            Self::Set(..) => 13,
            Self::Record(..) => 14,
            Self::Enum(..) => 15,
            Self::Function(..) => 16,
            Self::Task(..) => 17,
            Self::Error => 18,
            Self::Handle(..) => 19,
            Self::Union(..) => 20,
            Self::Json => 21,
        }
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

use crate::sites::*;
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

impl CodecVariant for Unit {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Main => 0,
            Self::Function(..) => 1,
            Self::Library(..) => 2,
        }
    }
}

impl CodecVariant for TaskIdentity {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Main => 0,
            Self::Spawned(..) => 1,
        }
    }
}

use crate::{LimitCause, TurnControlKind};
#[doc(hidden)]
impl CodecVariant for TurnControlKind {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::Finish => 0,
            Self::SwitchAgentFrame => 1,
        }
    }
}
#[doc(hidden)]
impl CodecVariant for LimitCause {
    fn codec_variant(&self) -> u32 {
        match self {
            Self::ExecutionSlice => 0,
            Self::ExecutionTotal => 1,
            Self::WaitDeadline => 2,
        }
    }
}

use crate::providers::PluginCallbackIdentity;
impl TokenCodec for PluginCallbackIdentity {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        out.text(&self.owner);
        out.text(&self.key);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            owner: input.text()?.to_owned(),
            key: input.text()?.to_owned(),
        })
    }
}

impl TokenCodec for crate::LinkId {
    fn encode(&self, out: &mut TokenWriter) {
        out.bytes(self.as_bytes());
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let bytes = input
            .bytes()?
            .try_into()
            .map_err(|_| TokenError::Invalid("a LinkId is 32 bytes".into()))?;
        Ok(Self::from_bytes(bytes))
    }
}
impl TokenCodec for crate::PackageId {
    fn encode(&self, out: &mut TokenWriter) {
        out.bytes(self.as_bytes());
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let bytes = input
            .bytes()?
            .try_into()
            .map_err(|_| TokenError::Invalid("a PackageId is 32 bytes".into()))?;
        Ok(Self::from_bytes(bytes))
    }
}
impl TokenCodec for crate::ImportSlot {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        u32::decode(input).map(Self)
    }
}
impl TokenCodec for crate::OperationUse {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.link.encode(out);
        self.slot.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            link: crate::LinkId::decode(input)?,
            slot: crate::ImportSlot::decode(input)?,
        })
    }
}
impl TokenCodec for crate::CallableId {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.link.encode(out);
        self.function.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            link: crate::LinkId::decode(input)?,
            function: crate::FunctionId::decode(input)?,
        })
    }
}

impl TokenCodec for crate::HelperLibraryId {
    fn encode(&self, out: &mut TokenWriter) {
        out.bytes(self.as_bytes());
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let bytes = input
            .bytes()?
            .try_into()
            .map_err(|_| TokenError::Invalid("a HelperLibraryId is 32 bytes".into()))?;
        Ok(Self::from_bytes(bytes))
    }
}
