//! Object records (cutover contract §6): an object's body in page form,
//! every member either an immediate or another object by id, and the exact
//! set of ids it references.

use std::sync::Arc;

use lash_kernel_doc::linked_site::LinkedSite;
use lash_kernel_doc::{Float, Handle, Integer};
use lash_kernel_value::Name;
use lash_kernel_value::content_ids::LinkId;
use lash_kernel_value::names::CallableId;
use lash_kernel_value::sites::Site;
use lash_kernel_value::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

use super::ids::ObjectId;

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
}

impl Record {
    /// Every id the record references, once each, ascending: its exact
    /// edge set.
    pub fn edges(&self) -> Vec<ObjectId> {
        let mut edges = Vec::new();
        let mut value = |value: &RunValue| edges.extend(value.id());
        match self {
            Self::List(values) | Self::Tuple(values) | Self::Set(values) => {
                values.iter().for_each(&mut value);
            }
            Self::Map(pairs) | Self::Record(pairs) => {
                for (key, member) in pairs {
                    value(key);
                    value(member);
                }
            }
            Self::Cell(member) => value(member),
            Self::Error {
                kind,
                message,
                data,
            } => {
                value(kind);
                value(message);
                value(data);
            }
            Self::Closure { captures, .. } => edges.extend(captures.iter().copied()),
            Self::BigInt(_)
            | Self::Timestamp(_)
            | Self::LongText(_)
            | Self::Bytes(_)
            | Self::Function(_)
            | Self::Handle(_) => {}
        }
        edges.sort_unstable();
        edges.dedup();
        edges
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
            _ => return Err(TokenError::UnsupportedVariant { variant: tag }),
        })
    }
}
