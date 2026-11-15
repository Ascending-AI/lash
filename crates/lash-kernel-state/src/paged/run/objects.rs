//! Object records (cutover contract §6): an object's body in page form,
//! every member either an immediate or another object by id, and the exact
//! set of ids it references.

use std::sync::Arc;

use lash_kernel_doc::heap::HeapFault;
use lash_kernel_doc::heap::{IntSummary, NormalKey, PAYLOAD_LEAF_BYTES, PayloadRoot, TextSummary};
use lash_kernel_doc::heap_values::text_summary;
use lash_kernel_doc::linked_site::LinkedSite;
use lash_kernel_doc::{Float, Handle, Integer};
use lash_kernel_value::Name;
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::CallableId;
use lash_kernel_value::sites::Site;
use lash_kernel_value::tokens::{
    TokenCodec, TokenError, TokenReader, TokenWriter, decode_body, encode_body,
};

use super::super::tree::Access;
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
    /// A closure's shared variable, held in a frame slot while its
    /// declaring frame runs (`K-CLO-001`).
    Cell,
}

impl ObjectKind {
    const ALL: [Self; 14] = [
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
        Self::Cell,
    ];

    fn tag(self) -> u64 {
        Self::ALL.iter().position(|kind| *kind == self).unwrap_or(0) as u64
    }

    /// Whether the kind is mutable: identity a run observes.
    pub fn mutable(self) -> bool {
        matches!(
            self,
            Self::List | Self::Map | Self::Set | Self::Record | Self::Closure | Self::Cell
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

/// A closure's code as its row holds it: inline within
/// [`SCALAR_INLINE_MAX`] token bytes, else its canonical tokens as a
/// payload tree, so no closure row outgrows its page whatever its capture
/// names and site. The directory places the code as it writes the row and
/// reads it back as it reads the row ([`super::directory::Directory`]), so
/// a row read through it holds its code inline.
#[derive(Clone, Debug, PartialEq)]
pub enum Code {
    Inline(ClosureCode),
    /// The code's token bytes, past [`SCALAR_INLINE_MAX`], and their tree.
    Paged {
        bytes: u64,
        root: PayloadRoot,
    },
}

impl Code {
    /// `code` as its row holds it: inline within [`SCALAR_INLINE_MAX`]
    /// token bytes, else written as a payload tree.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn place(access: &Access<'_>, code: ClosureCode) -> Result<Self, HeapFault> {
        let tokens = encode_body(&code);
        if tokens.len() as u64 <= SCALAR_INLINE_MAX {
            return Ok(Self::Inline(code));
        }
        Ok(Self::Paged {
            bytes: tokens.len() as u64,
            root: crate::paged::payload::build(access, &tokens, false)?,
        })
    }

    /// The code, read from its pages if it is paged, every edge checked
    /// against the root id.
    ///
    /// # Errors
    ///
    /// The page port's faults; a tree that disagrees with its count or
    /// identity, or whose tokens are no closure code.
    pub fn read(&self, access: &Access<'_>) -> Result<ClosureCode, HeapFault> {
        let (bytes, root) = match self {
            Self::Inline(code) => return Ok(code.clone()),
            Self::Paged { bytes, root } => (*bytes, *root),
        };
        let summary = TextSummary {
            utf8_bytes: bytes,
            chars: 0,
            utf16_units: 0,
        };
        let tokens = super::super::payload::copy(access, root, summary, false)?;
        decode_body(&tokens).map_err(|error| HeapFault::Corrupt {
            page: root.page.id,
            reason: format!("a closure's code payload holds no code: {error}"),
        })
    }

    /// The code of a row read through its directory.
    ///
    /// # Errors
    ///
    /// [`HeapFault::Malformed`] for code still on its pages.
    pub fn inline(&self) -> Result<&ClosureCode, HeapFault> {
        match self {
            Self::Inline(code) => Ok(code),
            Self::Paged { .. } => Err(HeapFault::Malformed(
                "a closure's code is read through its directory".into(),
            )),
        }
    }

    /// As [`Self::inline`], to change it.
    ///
    /// # Errors
    ///
    /// As [`Self::inline`].
    pub fn inline_mut(&mut self) -> Result<&mut ClosureCode, HeapFault> {
        match self {
            Self::Inline(code) => Ok(code),
            Self::Paged { .. } => Err(HeapFault::Malformed(
                "a closure's code is read through its directory".into(),
            )),
        }
    }

    /// The code inline, taken.
    ///
    /// # Errors
    ///
    /// As [`Self::inline`].
    pub fn into_inline(self) -> Result<ClosureCode, HeapFault> {
        match self {
            Self::Inline(code) => Ok(code),
            Self::Paged { .. } => Err(HeapFault::Malformed(
                "a closure's code is read through its directory".into(),
            )),
        }
    }
}

impl TokenCodec for Code {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Inline(code) => {
                out.sum(0, 1);
                code.encode(out);
            }
            Self::Paged { bytes, root } => {
                out.sum(1, 2);
                out.unsigned(*bytes);
                root.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (tag, fields) = input.sum()?;
        match tag {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Ok(Self::Inline(ClosureCode::decode(input)?))
            }
            1 => {
                TokenReader::expect_fields(fields, 2)?;
                let bytes = input.unsigned()?;
                if bytes <= SCALAR_INLINE_MAX {
                    return Err(TokenError::Invalid(
                        "a closure's code within one payload leaf is paged".into(),
                    ));
                }
                Ok(Self::Paged {
                    bytes,
                    root: PayloadRoot::decode(input)?,
                })
            }
            _ => Err(TokenError::UnsupportedVariant { variant: tag }),
        }
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
        code: Code,
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
    /// A text of at most [`PAYLOAD_LEAF_BYTES`] bytes; a longer one is
    /// [`Self::Paged`].
    LongText(Arc<str>),
    /// Bytes, at most [`PAYLOAD_LEAF_BYTES`]; longer ones are
    /// [`Self::Paged`].
    Bytes(Arc<[u8]>),
    /// A text or bytes past one payload leaf: its canonical payload tree
    /// ([`crate::paged::payload`]), so no row outgrows its page.
    Paged(Paged),
    Function(CallableId),
    /// A host handle whose kind and id take at most
    /// [`SCALAR_INLINE_MAX`] token bytes; a longer one is
    /// [`Self::Scalar`].
    Handle(Handle),
    /// A big integer, a big timestamp or a host handle past
    /// [`SCALAR_INLINE_MAX`] token bytes: its canonical tokens as a payload
    /// tree, so no row outgrows its page.
    Scalar(ScalarPayload),
    /// A large list or table: its index, its chunks on their own pages
    /// ([`super::chunked`]).
    Chunked(Box<Chunked>),
}

impl Record {
    /// The kind of object the record holds, whatever form its row takes.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::List(_) => ObjectKind::List,
            Self::Tuple(_) => ObjectKind::Tuple,
            Self::Set(_) => ObjectKind::Set,
            Self::Map(_) => ObjectKind::Map,
            Self::Record(_) => ObjectKind::Record,
            Self::Closure { .. } => ObjectKind::Closure,
            Self::Cell(_) => ObjectKind::Cell,
            Self::Error { .. } => ObjectKind::Error,
            Self::BigInt(_) => ObjectKind::BigInt,
            Self::Timestamp(_) => ObjectKind::Timestamp,
            Self::LongText(_) => ObjectKind::LongText,
            Self::Bytes(_) => ObjectKind::Bytes,
            Self::Paged(paged) => paged.kind(),
            Self::Function(_) => ObjectKind::Function,
            Self::Handle(_) => ObjectKind::Handle,
            Self::Scalar(scalar) => scalar.kind,
            Self::Chunked(chunked) => chunked.kind,
        }
    }

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
            | Self::Paged(_)
            | Self::Scalar(_)
            | Self::Function(_)
            | Self::Handle(_)
            | Self::Chunked(_) => {}
        }
        values
    }
}

/// A text's or bytes' payload tree: whether it is text, its summary (bytes'
/// counts only their bytes), and its root page with the identity it
/// commits to, which is the value's key (a paged payload is always longer
/// than `identity::FLAT_MAX`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Paged {
    pub text: bool,
    pub summary: TextSummary,
    pub root: PayloadRoot,
}

impl Paged {
    pub fn kind(&self) -> ObjectKind {
        if self.text {
            ObjectKind::LongText
        } else {
            ObjectKind::Bytes
        }
    }

    /// `bytes`, a text's when `text`, written as a payload tree.
    ///
    /// # Errors
    ///
    /// The page port's faults; a text that is no UTF-8.
    pub fn write(access: &Access<'_>, bytes: &[u8], text: bool) -> Result<Self, HeapFault> {
        let summary = if text {
            text_summary(
                std::str::from_utf8(bytes)
                    .map_err(|error| HeapFault::Malformed(error.to_string()))?,
            )
        } else {
            TextSummary {
                utf8_bytes: bytes.len() as u64,
                chars: 0,
                utf16_units: 0,
            }
        };
        Ok(Self {
            text,
            summary,
            root: crate::paged::payload::build(access, bytes, text)?,
        })
    }

    /// The head this payload's object is named by: its counts and, being
    /// longer than `identity::FLAT_MAX`, the root id as its key. What a
    /// load compares with the referrers' head before reading a page, and
    /// what a seal compares with a head already certified.
    pub fn head(&self) -> Head {
        let normal = NormalKey::Digest(self.root.id);
        if self.text {
            Head::LongText {
                summary: self.summary,
                normal,
            }
        } else {
            Head::Bytes {
                len: self.summary.utf8_bytes,
                normal,
            }
        }
    }

    /// The whole payload, read from its pages, each level's siblings in
    /// one batch, every edge checked against the root id.
    ///
    /// # Errors
    ///
    /// The page port's faults, and a tree that disagrees with the summary.
    pub fn read(&self, access: &Access<'_>) -> Result<Vec<u8>, HeapFault> {
        super::super::payload::copy(access, self.root, self.summary, self.text)
    }

    /// The root node of this payload alone, checked against the counts and
    /// the identity this edge commits to: a leaf's bytes, or the children,
    /// each a payload of its own whose edge the read certified. A reader
    /// that descends through them checks every edge it consumes.
    ///
    /// # Errors
    ///
    /// The page port's faults, and a node its edge does not certify.
    pub fn node(&self, access: &Access<'_>) -> Result<PagedNode, HeapFault> {
        use super::super::payload::{PayloadNode, node};
        Ok(match node(access, self.root, self.summary, self.text)? {
            PayloadNode::Leaf(bytes) => PagedNode::Leaf(bytes),
            PayloadNode::Index(children) => PagedNode::Index(
                children
                    .into_iter()
                    .map(|(root, summary)| Self {
                        text: self.text,
                        summary,
                        root,
                    })
                    .collect(),
            ),
        })
    }
}

/// One node of a payload tree ([`Paged::node`]).
#[derive(Clone, Debug)]
pub enum PagedNode {
    Leaf(Vec<u8>),
    Index(Vec<Paged>),
}

impl TokenCodec for Paged {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.text.encode(out);
        self.summary.encode(out);
        self.root.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let paged = Self {
            text: bool::decode(input)?,
            summary: TextSummary::decode(input)?,
            root: PayloadRoot::decode(input)?,
        };
        if paged.summary.utf8_bytes <= PAYLOAD_LEAF_BYTES {
            return Err(TokenError::Invalid(
                "a payload within one leaf is paged".into(),
            ));
        }
        Ok(paged)
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
            Self::Paged(paged) => {
                out.sum(15, 1);
                paged.encode(out);
            }
            Self::Scalar(scalar) => {
                out.sum(16, 1);
                scalar.encode(out);
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
                    code: Code::decode(input)?,
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
                Self::BigInt(inline(Integer::decode(input)?)?)
            }
            9 => {
                expect(1)?;
                Self::Timestamp(inline(Integer::decode(input)?)?)
            }
            10 => {
                expect(1)?;
                let text = input.text()?;
                if text.len() as u64 > PAYLOAD_LEAF_BYTES {
                    return Err(TokenError::Invalid(
                        "a text past one payload leaf is inline".into(),
                    ));
                }
                Self::LongText(text.into())
            }
            11 => {
                expect(1)?;
                let bytes = input.bytes()?;
                if bytes.len() as u64 > PAYLOAD_LEAF_BYTES {
                    return Err(TokenError::Invalid(
                        "bytes past one payload leaf are inline".into(),
                    ));
                }
                Self::Bytes(bytes.into())
            }
            12 => {
                expect(1)?;
                Self::Function(CallableId::decode(input)?)
            }
            13 => {
                expect(1)?;
                Self::Handle(inline(Handle::decode(input)?)?)
            }
            14 => {
                expect(1)?;
                Self::Chunked(Box::new(Chunked::decode(input)?))
            }
            15 => {
                expect(1)?;
                Self::Paged(Paged::decode(input)?)
            }
            16 => {
                expect(1)?;
                Self::Scalar(ScalarPayload::decode(input)?)
            }
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}

/// The most token bytes a big integer, a big timestamp or a host handle
/// holds in its own row; a longer one is a [`ScalarPayload`]. One payload
/// leaf, as for a text or bytes.
pub const SCALAR_INLINE_MAX: u64 = PAYLOAD_LEAF_BYTES;

/// A scalar a row holds inline: one within [`SCALAR_INLINE_MAX`], so its
/// form is canonical.
fn inline<T: TokenCodec>(scalar: T) -> Result<T, TokenError> {
    if encode_body(&scalar).len() as u64 > SCALAR_INLINE_MAX {
        return Err(TokenError::Invalid(
            "a scalar past one payload leaf is inline".into(),
        ));
    }
    Ok(scalar)
}

/// A big integer's, a big timestamp's or a host handle's canonical tokens
/// as a payload tree: its kind, the tree's counts (bytes count only their
/// bytes) and its root page with the identity it commits to. Its head
/// carries the scalar's own summary and key, which a load checks the
/// decoded scalar against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScalarPayload {
    pub kind: ObjectKind,
    pub summary: TextSummary,
    pub root: PayloadRoot,
}

impl ScalarPayload {
    /// `record`, a big integer, timestamp or host handle, as its row holds
    /// it: inline within [`SCALAR_INLINE_MAX`] token bytes, else written as
    /// a payload tree. Any other record is returned as it is.
    ///
    /// # Errors
    ///
    /// The page port's faults.
    pub fn place(access: &Access<'_>, record: Record) -> Result<Record, HeapFault> {
        let (kind, tokens) = match &record {
            Record::BigInt(int) => (ObjectKind::BigInt, encode_body(int)),
            Record::Timestamp(int) => (ObjectKind::Timestamp, encode_body(int)),
            Record::Handle(handle) => (ObjectKind::Handle, encode_body(handle)),
            _ => return Ok(record),
        };
        if tokens.len() as u64 <= SCALAR_INLINE_MAX {
            return Ok(record);
        }
        Ok(Record::Scalar(Self {
            kind,
            summary: TextSummary {
                utf8_bytes: tokens.len() as u64,
                chars: 0,
                utf16_units: 0,
            },
            root: crate::paged::payload::build(access, &tokens, false)?,
        }))
    }

    /// The scalar record this payload holds, read from its pages, every
    /// edge checked against the root id.
    ///
    /// # Errors
    ///
    /// The page port's faults; a tree that disagrees with its counts or
    /// identity, or whose tokens are no canonical scalar of its kind.
    pub fn read(&self, access: &Access<'_>) -> Result<Record, HeapFault> {
        let tokens = super::super::payload::copy(access, self.root, self.summary, false)?;
        let corrupt = |error: TokenError| HeapFault::Corrupt {
            page: self.root.page.id,
            reason: format!("a scalar payload holds no scalar of its kind: {error}"),
        };
        let record = match self.kind {
            ObjectKind::BigInt => Record::BigInt(decode_body(&tokens).map_err(corrupt)?),
            ObjectKind::Timestamp => Record::Timestamp(decode_body(&tokens).map_err(corrupt)?),
            _ => Record::Handle(decode_body(&tokens).map_err(corrupt)?),
        };
        Ok(record)
    }
}

impl TokenCodec for ScalarPayload {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.kind.encode(out);
        out.unsigned(self.summary.utf8_bytes);
        self.root.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let kind = ObjectKind::decode(input)?;
        let bytes = input.unsigned()?;
        if !matches!(
            kind,
            ObjectKind::BigInt | ObjectKind::Timestamp | ObjectKind::Handle
        ) {
            return Err(TokenError::Invalid(
                "a scalar payload holds no big integer, timestamp or handle".into(),
            ));
        }
        if bytes <= SCALAR_INLINE_MAX {
            return Err(TokenError::Invalid(
                "a scalar within one payload leaf is paged".into(),
            ));
        }
        Ok(Self {
            kind,
            summary: TextSummary {
                utf8_bytes: bytes,
                chars: 0,
                utf16_units: 0,
            },
            root: PayloadRoot::decode(input)?,
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
/// Every head is bounded by construction. A function's head holds the
/// whole callable id, so it opens resident; a host handle's holds its kind
/// and id lengths and its content: its tokens whole within
/// [`NormalKey::INLINE_MAX`], so it opens resident, else their digest, so
/// it opens cold like a big integer.
#[derive(Clone, Debug, PartialEq)]
pub enum Head {
    /// `normal` is `None` exactly when the tuple is no legal key; `root`
    /// is its content root ([`crate::paged::identity::TupleContent`]),
    /// which every page of a chunked tuple is checked against.
    Tuple {
        len: u64,
        shape: Summary,
        normal: Option<NormalKey>,
        root: [u8; 32],
    },
    /// `content` is the digest of the error's kind, message and data
    /// ([`crate::paged::identity::error_content`]).
    Error {
        shape: Summary,
        content: [u8; 32],
    },
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
    Handle {
        kind_len: u64,
        id_len: u64,
        content: NormalKey,
    },
}

/// The domain of a long host handle's content digest.
const HANDLE_DIGEST: &[u8] = b"lash.handle.v1\0";

impl Head {
    /// The head of host handle `handle`.
    #[must_use]
    pub fn handle(handle: &Handle) -> Self {
        let tokens = encode_body(handle);
        let content = if tokens.len() <= NormalKey::INLINE_MAX {
            NormalKey::Inline(tokens)
        } else {
            NormalKey::Digest(lash_kernel_doc::heap_values::sha256(&[
                HANDLE_DIGEST,
                &tokens,
            ]))
        };
        Self::Handle {
            kind_len: handle.kind.len() as u64,
            id_len: handle.id.len() as u64,
            content,
        }
    }

    /// The host handle a handle head holds whole, when it does.
    #[must_use]
    pub fn resident_handle(&self) -> Option<Handle> {
        match self {
            Self::Handle {
                content: NormalKey::Inline(tokens),
                ..
            } => decode_body(tokens).ok(),
            _ => None,
        }
    }

    /// The kind of object the head certifies.
    pub fn kind(&self) -> ObjectKind {
        match self {
            Self::Tuple { .. } => ObjectKind::Tuple,
            Self::Error { .. } => ObjectKind::Error,
            Self::BigInt { .. } => ObjectKind::BigInt,
            Self::Timestamp { .. } => ObjectKind::Timestamp,
            Self::LongText { .. } => ObjectKind::LongText,
            Self::Bytes { .. } => ObjectKind::Bytes,
            Self::Function(_) => ObjectKind::Function,
            Self::Handle { .. } => ObjectKind::Handle,
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
            Self::Error { .. } | Self::Function(_) | Self::Handle { .. } => None,
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

fn digest(input: &mut TokenReader<'_>) -> Result<[u8; 32], TokenError> {
    input
        .bytes()?
        .try_into()
        .map_err(|_| TokenError::Invalid("a head's content digest is not 32 bytes".into()))
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
            Self::Tuple {
                len,
                shape,
                normal,
                root,
            } => {
                // One identity: the root, from which a tree-keyed tuple's
                // normal form is its digest, or the flat form, from which
                // decoding derives the root. Neither is carried twice.
                out.sum(0, 3);
                out.unsigned(*len);
                encode_shape(shape, out);
                match normal {
                    None => {
                        out.sum(0, 1);
                        out.bytes(root);
                    }
                    Some(NormalKey::Digest(_)) => {
                        // The root is the identity; the digest is never
                        // written, so no head can name one tuple's key
                        // and another's root.
                        out.sum(1, 1);
                        out.bytes(root);
                    }
                    Some(NormalKey::Inline(flat)) => {
                        out.sum(2, 1);
                        out.bytes(flat);
                    }
                }
            }
            Self::Error { shape, content } => {
                out.sum(1, 2);
                encode_shape(shape, out);
                out.bytes(content);
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
            Self::Handle {
                kind_len,
                id_len,
                content,
            } => {
                out.sum(7, 3);
                out.unsigned(*kind_len);
                out.unsigned(*id_len);
                encode_normal(content, out);
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
                let (form, fields) = input.sum()?;
                TokenReader::expect_fields(fields, 1)?;
                let (normal, root) = match form {
                    0 => (None, digest(input)?),
                    1 => {
                        let root = digest(input)?;
                        (Some(NormalKey::Digest(root)), root)
                    }
                    2 => {
                        let flat = input.bytes()?;
                        if flat.len() > NormalKey::INLINE_MAX {
                            return Err(TokenError::Invalid(
                                "an inline normal form exceeds the inline bound".into(),
                            ));
                        }
                        let root = crate::paged::identity::flat_root(len, flat)?;
                        (Some(NormalKey::Inline(flat.to_vec())), root)
                    }
                    _ => return Err(TokenError::UnsupportedVariant { variant: form }),
                };
                Self::Tuple {
                    len,
                    shape,
                    normal,
                    root,
                }
            }
            1 => {
                expect(2)?;
                Self::Error {
                    shape: decode_shape(input)?,
                    content: digest(input)?,
                }
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
                expect(3)?;
                let kind_len = input.unsigned()?;
                let id_len = input.unsigned()?;
                let content = decode_normal(input)?;
                let whole = matches!(content, NormalKey::Inline(_));
                let head = Self::Handle {
                    kind_len,
                    id_len,
                    content,
                };
                // A whole handle's head is the one it gives; a digest
                // stands for one too long to hold whole, which a load
                // checks the row against.
                if whole
                    && head
                        .resident_handle()
                        .is_none_or(|handle| Self::handle(&handle) != head)
                {
                    return Err(TokenError::Invalid(
                        "a handle head's content disagrees with its lengths".into(),
                    ));
                }
                head
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
