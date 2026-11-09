//! Object records (cutover contract §6): an object's body in page form,
//! every member either an immediate or another object by id, and the exact
//! set of ids it references.

use std::sync::Arc;

use lash_kernel_doc::heap::{IntSummary, NormalKey, TextSummary};
use lash_kernel_doc::linked_site::LinkedSite;
use lash_kernel_doc::{Float, Handle, Integer};
use lash_kernel_value::Name;
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::CallableId;
use lash_kernel_value::sites::Site;
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

use super::chunked::Chunked;
use super::ids::ObjectId;
use crate::arena::Summary;

/// What kind of object an id names, as the value that holds it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ObjectKind {
    List,
    Map,
    Set,
    Record,
    Closure,
    Tuple,
    Error,
    BigInt,
    Timestamp,
    LongText,
    Bytes,
    Function,
    Handle,
}

impl ObjectKind {
    const ALL: [Self; 13] = [
        Self::List,
        Self::Map,
        Self::Set,
        Self::Record,
        Self::Closure,
        Self::Tuple,
        Self::Error,
        Self::BigInt,
        Self::Timestamp,
        Self::LongText,
        Self::Bytes,
        Self::Function,
        Self::Handle,
    ];

    fn tag(self) -> u64 {
        Self::ALL.iter().position(|kind| *kind == self).unwrap_or(0) as u64
    }

    /// Whether the kind is mutable: identity a run observes.
    pub fn mutable(self) -> bool {
        matches!(
            self,
            Self::List | Self::Map | Self::Set | Self::Record | Self::Closure
        )
    }
}

impl TokenCodec for ObjectKind {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(self.tag());
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let tag = input.unsigned()?;
        usize::try_from(tag)
            .ok()
            .and_then(|tag| Self::ALL.get(tag).copied())
            .ok_or(TokenError::UnsupportedVariant {
                variant: u32::try_from(tag).unwrap_or(u32::MAX),
            })
    }
}

/// A value in page form.
#[derive(Clone, Debug, PartialEq)]
pub enum RunValue {
    Null,
    Absent,
    Bool(bool),
    Int(i64),
    Float(Float),
    /// A short text, by value.
    Text(Arc<str>),
    Timestamp(i64),
    Task(u64),
    TaskRef(u64),
    /// An object, of the kind the value names.
    Object(ObjectKind, ObjectId),
    /// A ref to an object (`K-KEY-004`), whatever its kind.
    Ref(ObjectId),
}

impl RunValue {
    /// The object the value names, if it names one.
    pub fn id(&self) -> Option<ObjectId> {
        match self {
            Self::Object(_, id) | Self::Ref(id) => Some(*id),
            _ => None,
        }
    }
}

impl TokenCodec for RunValue {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Null => out.sum(0, 0),
            Self::Absent => out.sum(1, 0),
            Self::Bool(value) => {
                out.sum(2, 1);
                out.bool(*value);
            }
            Self::Int(value) => {
                out.sum(3, 1);
                out.signed(*value);
            }
            Self::Float(value) => {
                out.sum(4, 1);
                value.encode(out);
            }
            Self::Text(text) => {
                out.sum(5, 1);
                out.text(text);
            }
            Self::Timestamp(value) => {
                out.sum(6, 1);
                out.signed(*value);
            }
            Self::Task(task) => {
                out.sum(7, 1);
                out.unsigned(*task);
            }
            Self::TaskRef(task) => {
                out.sum(8, 1);
                out.unsigned(*task);
            }
            Self::Object(kind, id) => {
                out.sum(9, 2);
                kind.encode(out);
                id.encode(out);
            }
            Self::Ref(id) => {
                out.sum(10, 1);
                id.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        let expect = |count| TokenReader::expect_fields(fields, count);
        Ok(match tag {
            0 => {
                expect(0)?;
                Self::Null
            }
            1 => {
                expect(0)?;
                Self::Absent
            }
            2 => {
                expect(1)?;
                Self::Bool(input.bool()?)
            }
            3 => {
                expect(1)?;
                Self::Int(input.signed()?)
            }
            4 => {
                expect(1)?;
                Self::Float(Float::decode(input)?)
            }
            5 => {
                expect(1)?;
                Self::Text(input.text()?.into())
            }
            6 => {
                expect(1)?;
                Self::Timestamp(input.signed()?)
            }
            7 => {
                expect(1)?;
                Self::Task(input.unsigned()?)
            }
            8 => {
                expect(1)?;
                Self::TaskRef(input.unsigned()?)
            }
            9 => {
                expect(2)?;
                Self::Object(ObjectKind::decode(input)?, ObjectId::decode(input)?)
            }
            10 => {
                expect(1)?;
                Self::Ref(ObjectId::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

/// A closure's code: what every closure of one expression shares.
#[derive(Clone, Debug, PartialEq)]
pub struct ClosureCode {
    pub capture_names: Vec<Name>,
    pub callable: CallableId,
    pub link: LinkId,
    pub site: Site,
}

impl ClosureCode {
    /// The closure's expression coordinate.
    pub fn linked_site(&self) -> LinkedSite {
        LinkedSite {
            link: self.link,
            site: self.site.clone(),
        }
    }
}

impl TokenCodec for ClosureCode {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.capture_names.encode(out);
        self.callable.encode(out);
        self.link.encode(out);
        self.site.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            capture_names: Vec::decode(input)?,
            callable: CallableId::decode(input)?,
            link: LinkId::decode(input)?,
            site: Site::decode(input)?,
        })
    }
}

/// An object's body in page form.
#[derive(Clone, Debug, PartialEq)]
pub enum Record {
    List(Vec<RunValue>),
    Tuple(Vec<RunValue>),
    Set(Vec<RunValue>),
    /// Entries in insertion order, key then value.
    Map(Vec<(RunValue, RunValue)>),
    Record(Vec<(RunValue, RunValue)>),
    Closure {
        code: ClosureCode,
        /// The cells of its captured variables, in capture order.
        captures: Vec<ObjectId>,
    },
    /// A variable a closure shares (`K-CLO-001`).
    Cell(RunValue),
    Error {
        kind: RunValue,
        message: RunValue,
        data: RunValue,
    },
    BigInt(Integer),
    Timestamp(Integer),
    LongText(Arc<str>),
    Bytes(Arc<[u8]>),
    Function(CallableId),
    Handle(Handle),
    /// A large list or table: its index, its chunks on their own pages
    /// ([`super::chunked`]).
    Chunked(Box<Chunked>),
}

impl Record {
    /// Every id the record references, once each, ascending: its exact
    /// edge set.
    pub fn edges(&self) -> Vec<ObjectId> {
        if let Self::Chunked(_) = self {
            // Its edges are in its edge pages.
            return Vec::new();
        }
        let mut edges: Vec<ObjectId> = self.values().into_iter().filter_map(RunValue::id).collect();
        if let Self::Closure { captures, .. } = self {
            edges.extend(captures.iter().copied());
        }
        edges.sort_unstable();
        edges.dedup();
        edges
    }

    /// Every value the record holds, in member order.
    pub fn values(&self) -> Vec<&RunValue> {
        let mut values = Vec::new();
        match self {
            Self::List(members) | Self::Tuple(members) | Self::Set(members) => {
                values.extend(members);
            }
            Self::Map(pairs) | Self::Record(pairs) => {
                for (key, member) in pairs {
                    values.push(key);
                    values.push(member);
                }
            }
            Self::Cell(member) => values.push(member),
            Self::Error {
                kind,
                message,
                data,
            } => values.extend([kind, message, data]),
            Self::Closure { .. }
            | Self::BigInt(_)
            | Self::Timestamp(_)
            | Self::LongText(_)
            | Self::Bytes(_)
            | Self::Function(_)
            | Self::Handle(_)
            | Self::Chunked(_) => {}
        }
        values
    }
}

fn encode_pairs(pairs: &[(RunValue, RunValue)], out: &mut TokenWriter) {
    out.seq(pairs.len() as u64 * 2);
    for (key, value) in pairs {
        key.encode(out);
        value.encode(out);
    }
}

fn decode_pairs(input: &mut TokenReader<'_>) -> Result<Vec<(RunValue, RunValue)>, TokenError> {
    let flat = Vec::<RunValue>::decode(input)?;
    if flat.len() % 2 != 0 {
        return Err(TokenError::Invalid(
            "a table record's last key has no value".into(),
        ));
    }
    let mut pairs = Vec::with_capacity(flat.len() / 2);
    let mut flat = flat.into_iter();
    while let (Some(key), Some(value)) = (flat.next(), flat.next()) {
        pairs.push((key, value));
    }
    Ok(pairs)
}

impl TokenCodec for Record {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::List(values) => {
                out.sum(0, 1);
                values.encode(out);
            }
            Self::Tuple(values) => {
                out.sum(1, 1);
                values.encode(out);
            }
            Self::Set(values) => {
                out.sum(2, 1);
                values.encode(out);
            }
            Self::Map(pairs) => {
                out.sum(3, 1);
                encode_pairs(pairs, out);
            }
            Self::Record(pairs) => {
                out.sum(4, 1);
                encode_pairs(pairs, out);
            }
            Self::Closure { code, captures } => {
                out.sum(5, 2);
                code.encode(out);
                captures.encode(out);
            }
            Self::Cell(value) => {
                out.sum(6, 1);
                value.encode(out);
            }
            Self::Error {
                kind,
                message,
                data,
            } => {
                out.sum(7, 3);
                kind.encode(out);
                message.encode(out);
                data.encode(out);
            }
            Self::BigInt(int) => {
                out.sum(8, 1);
                int.encode(out);
            }
            Self::Timestamp(int) => {
                out.sum(9, 1);
                int.encode(out);
            }
            Self::LongText(text) => {
                out.sum(10, 1);
                out.text(text);
            }
            Self::Bytes(bytes) => {
                out.sum(11, 1);
                out.bytes(bytes);
            }
            Self::Function(callable) => {
                out.sum(12, 1);
                callable.encode(out);
            }
            Self::Handle(handle) => {
                out.sum(13, 1);
                handle.encode(out);
            }
            Self::Chunked(chunked) => {
                out.sum(14, 1);
                chunked.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        let expect = |count| TokenReader::expect_fields(fields, count);
        Ok(match tag {
            0 => {
                expect(1)?;
                Self::List(Vec::decode(input)?)
            }
            1 => {
                expect(1)?;
                Self::Tuple(Vec::decode(input)?)
            }
            2 => {
                expect(1)?;
                Self::Set(Vec::decode(input)?)
            }
            3 => {
                expect(1)?;
                Self::Map(decode_pairs(input)?)
            }
            4 => {
                expect(1)?;
                Self::Record(decode_pairs(input)?)
            }
            5 => {
                expect(2)?;
                Self::Closure {
                    code: ClosureCode::decode(input)?,
                    captures: Vec::decode(input)?,
                }
            }
            6 => {
                expect(1)?;
                Self::Cell(RunValue::decode(input)?)
            }
            7 => {
                expect(3)?;
                Self::Error {
                    kind: RunValue::decode(input)?,
                    message: RunValue::decode(input)?,
                    data: RunValue::decode(input)?,
                }
            }
            8 => {
                expect(1)?;
                Self::BigInt(Integer::decode(input)?)
            }
            9 => {
                expect(1)?;
                Self::Timestamp(Integer::decode(input)?)
            }
            10 => {
                expect(1)?;
                Self::LongText(input.text()?.into())
            }
            11 => {
                expect(1)?;
                Self::Bytes(input.bytes()?.into())
            }
            12 => {
                expect(1)?;
                Self::Function(CallableId::decode(input)?)
            }
            13 => {
                expect(1)?;
                Self::Handle(Handle::decode(input)?)
            }
            14 => {
                expect(1)?;
                Self::Chunked(Box::new(Chunked::decode(input)?))
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

/// What a value certifies of the immutable object it names (lead ruling A,
/// `r57/astra-immutable-head.md`): the object's kind, the summaries a
/// reader may take without its payload, and its key's normal form. A row
/// holds the head of every immutable it references, so a load decodes one
/// row and the immutables it names open cold, true to their heads, with no
/// further read. A head is a certificate of one object: every copy of it
/// is derived from the same sealed object, and a load checks the body it
/// decodes against the head the object opened with.
///
/// A function's and a host handle's heads hold the whole object: what it
/// is is bounded, so it opens resident.
#[derive(Clone, Debug, PartialEq)]
pub enum Head {
    /// `normal` is `None` exactly when the tuple is no legal key.
    Tuple {
        len: u64,
        shape: Summary,
        normal: Option<NormalKey>,
    },
    Error(Summary),
    BigInt {
        summary: IntSummary,
        normal: NormalKey,
    },
    Timestamp {
        summary: IntSummary,
        normal: NormalKey,
    },
    LongText {
        summary: TextSummary,
        normal: NormalKey,
    },
    Bytes {
        len: u64,
        normal: NormalKey,
    },
    Function(CallableId),
    Handle(Handle),
}

impl Head {
    /// The kind of object the head certifies.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Tuple { .. } => ObjectKind::Tuple,
            Self::Error(_) => ObjectKind::Error,
            Self::BigInt { .. } => ObjectKind::BigInt,
            Self::Timestamp { .. } => ObjectKind::Timestamp,
            Self::LongText { .. } => ObjectKind::LongText,
            Self::Bytes { .. } => ObjectKind::Bytes,
            Self::Function(_) => ObjectKind::Function,
            Self::Handle(_) => ObjectKind::Handle,
        }
    }

    /// The normal form of the object as a key, when it is a legal one.
    pub fn normal(&self) -> Option<&NormalKey> {
        match self {
            Self::Tuple { normal, .. } => normal.as_ref(),
            Self::BigInt { normal, .. }
            | Self::Timestamp { normal, .. }
            | Self::LongText { normal, .. }
            | Self::Bytes { normal, .. } => Some(normal),
            Self::Error(_) | Self::Function(_) | Self::Handle(_) => None,
        }
    }
}

fn encode_shape(shape: &Summary, out: &mut TokenWriter) {
    out.seq(3);
    out.unsigned(u64::from(shape.depth));
    out.unsigned(shape.task_edges);
    out.bool(shape.has_edges);
}

fn decode_shape(input: &mut TokenReader<'_>) -> Result<Summary, TokenError> {
    input.expect_struct(3)?;
    let depth = u32::try_from(input.unsigned()?)
        .map_err(|_| TokenError::Invalid("a head's depth exceeds u32".into()))?;
    let task_edges = input.unsigned()?;
    let has_edges = input.bool()?;
    if depth == 0 || (task_edges > 0 && !has_edges) {
        return Err(TokenError::Invalid(
            "a head's depth or edge summary is impossible".into(),
        ));
    }
    Ok(Summary {
        depth,
        task_edges,
        has_edges,
    })
}

fn encode_normal(normal: &NormalKey, out: &mut TokenWriter) {
    match normal {
        NormalKey::Inline(tokens) => {
            out.sum(0, 1);
            out.bytes(tokens);
        }
        NormalKey::Digest(digest) => {
            out.sum(1, 1);
            out.bytes(digest);
        }
    }
}

fn decode_normal(input: &mut TokenReader<'_>) -> Result<NormalKey, TokenError> {
    let (tag, fields) = input.sum()?;
    TokenReader::expect_fields(fields, 1)?;
    match tag {
        0 => {
            let tokens = input.bytes()?;
            if tokens.len() > NormalKey::INLINE_MAX {
                return Err(TokenError::Invalid(
                    "an inline normal form exceeds the inline bound".into(),
                ));
            }
            Ok(NormalKey::Inline(tokens.to_vec()))
        }
        1 => Ok(NormalKey::Digest(input.bytes()?.try_into().map_err(
            |_| TokenError::Invalid("a normal form's digest is not 32 bytes".into()),
        )?)),
        _ => Err(TokenError::UnsupportedVariant { variant: tag }),
    }
}

fn encode_int(summary: &IntSummary, out: &mut TokenWriter) {
    out.seq(2);
    out.unsigned(summary.bits);
    out.bool(summary.negative);
}

/// A big integer's or timestamp's summary: it takes more than a word, so
/// its magnitude has at least 64 bits.
fn decode_int(input: &mut TokenReader<'_>) -> Result<IntSummary, TokenError> {
    input.expect_struct(2)?;
    let summary = IntSummary {
        bits: input.unsigned()?,
        negative: input.bool()?,
    };
    if summary.bits < 64 {
        return Err(TokenError::Invalid(
            "a big integer's head holds a word's magnitude".into(),
        ));
    }
    Ok(summary)
}

impl TokenCodec for Head {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Tuple { len, shape, normal } => {
                out.sum(0, 3);
                out.unsigned(*len);
                encode_shape(shape, out);
                match normal {
                    None => out.sum(0, 0),
                    Some(normal) => {
                        out.sum(1, 1);
                        encode_normal(normal, out);
                    }
                }
            }
            Self::Error(shape) => {
                out.sum(1, 1);
                encode_shape(shape, out);
            }
            Self::BigInt { summary, normal } => {
                out.sum(2, 2);
                encode_int(summary, out);
                encode_normal(normal, out);
            }
            Self::Timestamp { summary, normal } => {
                out.sum(3, 2);
                encode_int(summary, out);
                encode_normal(normal, out);
            }
            Self::LongText { summary, normal } => {
                out.sum(4, 2);
                out.seq(3);
                out.unsigned(summary.utf8_bytes);
                out.unsigned(summary.chars);
                out.unsigned(summary.utf16_units);
                encode_normal(normal, out);
            }
            Self::Bytes { len, normal } => {
                out.sum(5, 2);
                out.unsigned(*len);
                encode_normal(normal, out);
            }
            Self::Function(callable) => {
                out.sum(6, 1);
                callable.encode(out);
            }
            Self::Handle(handle) => {
                out.sum(7, 1);
                handle.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        let expect = |count| TokenReader::expect_fields(fields, count);
        Ok(match tag {
            0 => {
                expect(3)?;
                let len = input.unsigned()?;
                let shape = decode_shape(input)?;
                let (present, fields) = input.sum()?;
                let normal = match present {
                    0 => {
                        TokenReader::expect_fields(fields, 0)?;
                        None
                    }
                    1 => {
                        TokenReader::expect_fields(fields, 1)?;
                        Some(decode_normal(input)?)
                    }
                    _ => return Err(TokenError::UnsupportedVariant { variant: present }),
                };
                Self::Tuple { len, shape, normal }
            }
            1 => {
                expect(1)?;
                Self::Error(decode_shape(input)?)
            }
            2 => {
                expect(2)?;
                Self::BigInt {
                    summary: decode_int(input)?,
                    normal: decode_normal(input)?,
                }
            }
            3 => {
                expect(2)?;
                Self::Timestamp {
                    summary: decode_int(input)?,
                    normal: decode_normal(input)?,
                }
            }
            4 => {
                expect(2)?;
                input.expect_struct(3)?;
                let summary = TextSummary {
                    utf8_bytes: input.unsigned()?,
                    chars: input.unsigned()?,
                    utf16_units: input.unsigned()?,
                };
                // Every scalar takes one to four bytes and one or two
                // UTF-16 units, and a text that short is an immediate.
                if summary.utf8_bytes <= lash_kernel_doc::arena::ShortText::MAX as u64
                    || summary.chars > summary.utf8_bytes
                    || summary.utf8_bytes > summary.chars.saturating_mul(4)
                    || summary.utf16_units < summary.chars
                    || summary.utf16_units > summary.chars.saturating_mul(2)
                {
                    return Err(TokenError::Invalid(
                        "a text head's counts are impossible".into(),
                    ));
                }
                Self::LongText {
                    summary,
                    normal: decode_normal(input)?,
                }
            }
            5 => {
                expect(2)?;
                Self::Bytes {
                    len: input.unsigned()?,
                    normal: decode_normal(input)?,
                }
            }
            6 => {
                expect(1)?;
                Self::Function(CallableId::decode(input)?)
            }
            7 => {
                expect(1)?;
                Self::Handle(Handle::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

/// The heads of the immutables a row or a root names: ascending by id,
/// once each, exactly the ids its values name an immutable kind by
/// (record-local deduplication: a head repeated in one row is held once).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Heads(pub Vec<(ObjectId, Head)>);

impl Heads {
    /// The head of immutable `id`, if the row holds one.
    pub fn get(&self, id: ObjectId) -> Option<&Head> {
        self.0
            .binary_search_by_key(&id, |(held, _)| *held)
            .ok()
            .map(|at| &self.0[at].1)
    }

    /// Checks that the heads are exactly those of the immutables `values`
    /// name, each of the kind its values name it by.
    ///
    /// # Errors
    ///
    /// A head missing, extra, out of order, or of another kind than a
    /// value names its object by.
    pub fn check<'a>(&self, values: impl IntoIterator<Item = &'a RunValue>) -> Result<(), String> {
        if !self.0.windows(2).all(|pair| pair[0].0 < pair[1].0) {
            return Err("a row's heads are not ascending by id, once each".into());
        }
        let mut named = std::collections::BTreeMap::new();
        for value in values {
            if let RunValue::Object(kind, id) = value
                && !kind.mutable()
                && *named.entry(*id).or_insert(*kind) != *kind
            {
                return Err(format!("object {} is named as two kinds", id.0));
            }
        }
        if named.len() != self.0.len() {
            return Err("a row's heads are not exactly its immutables'".into());
        }
        for ((id, kind), (held, head)) in named.iter().zip(&self.0) {
            if id != held || head.kind() != *kind {
                return Err(format!(
                    "object {}'s head is missing or of another kind",
                    id.0
                ));
            }
        }
        Ok(())
    }
}

impl TokenCodec for Heads {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(self.0.len() as u64 * 2);
        for (id, head) in &self.0 {
            id.encode(out);
            head.encode(out);
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let count = input.seq()?;
        if count % 2 != 0 {
            return Err(TokenError::Invalid("a head has no id".into()));
        }
        let mut heads = Vec::with_capacity(usize::try_from(count / 2).unwrap_or(0).min(1024));
        for _ in 0..count / 2 {
            heads.push((ObjectId::decode(input)?, Head::decode(input)?));
        }
        Ok(Self(heads))
    }
}
