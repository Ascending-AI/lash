//! The canonical binary token grammar of checkpoint graph node bodies
//! (FIG-5887 §2).
//!
//! A node body is a stream of typed tokens, each a one-byte tag and its
//! payload. The tags and their encodings are fixed:
//!
//! | Tag | Encoding after the tag |
//! | --- | --- |
//! | `00` | unit/null |
//! | `01`, `02` | false, true |
//! | `03` | unsigned integer, minimal LEB128 |
//! | `04` | signed i64, zigzag then minimal LEB128 |
//! | `05` | arbitrary integer: sign byte (0 for zero or positive, 1 for negative), magnitude byte count, minimal little-endian magnitude |
//! | `06` | IEEE binary64, eight little-endian bytes; the only NaN is `0x7ff8000000000000` |
//! | `07` | UTF-8 text: byte count then the bytes |
//! | `08` | bytes: byte count then the bytes |
//! | `09` | sequence, tuple or struct: element count then the elements |
//! | `0a` | unordered map: entry count then key, value tokens in schema order |
//! | `0b` | child reference |
//! | `0c` | sum: variant number (u32), field count, then the fields |
//! | `0d` | none |
//! | `0e` | some, then one token |
//!
//! A child reference has two forms. In a **logical** body, the form typed
//! codecs read and write, it is the tag and the child's raw 32-byte digest.
//! In a **physical** body, the form stored inside a node envelope, it is the
//! tag and the minimal index of the child in the node's sorted child table.
//! The graph layer (`lash_core_store::checkpoint_graph`) converts between
//! them, places large text and bytes in leaves, and hashes envelopes; this
//! module never hashes.
//!
//! Every count and length is a u64 and is checked before it sizes an
//! allocation. A reader refuses every non-canonical spelling: an overlong or
//! overflowed varint, a non-minimal arbitrary integer, a NaN other than the
//! canonical one, an unknown tag, a truncated payload and trailing bytes.
//!
//! This module lives in the kernel's document crate, not in the store,
//! because the kernel crates encode their own state with it and may depend on
//! no other lash crate (`docs/kernel/design.md` §9). It is a cross-crate seam,
//! not part of the kernel's document vocabulary.

use std::fmt;

/// The greatest number of bytes a minimal LEB128 u64 takes.
pub const MAX_VARINT_BYTES: usize = 10;

/// The bit pattern of the one NaN a body may hold.
pub const CANONICAL_NAN_BITS: u64 = 0x7ff8_0000_0000_0000;

/// The bytes a child reference takes in a logical body: its tag and digest.
pub const LOGICAL_REF_BYTES: usize = 33;

/// A token's tag. The numbers are the format: never renumber one.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum Tag {
    Unit = 0x00,
    False = 0x01,
    True = 0x02,
    Unsigned = 0x03,
    Signed = 0x04,
    BigInteger = 0x05,
    Float = 0x06,
    Text = 0x07,
    Bytes = 0x08,
    Seq = 0x09,
    Map = 0x0a,
    Ref = 0x0b,
    Sum = 0x0c,
    None = 0x0d,
    Some = 0x0e,
}

impl Tag {
    /// The tag a byte spells, if any.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            0x00 => Self::Unit,
            0x01 => Self::False,
            0x02 => Self::True,
            0x03 => Self::Unsigned,
            0x04 => Self::Signed,
            0x05 => Self::BigInteger,
            0x06 => Self::Float,
            0x07 => Self::Text,
            0x08 => Self::Bytes,
            0x09 => Self::Seq,
            0x0a => Self::Map,
            0x0b => Self::Ref,
            0x0c => Self::Sum,
            0x0d => Self::None,
            0x0e => Self::Some,
            _ => return None,
        })
    }

    #[must_use]
    pub const fn byte(self) -> u8 {
        self as u8
    }
}

/// A child node's raw 32-byte digest, as a logical body names it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ChildRef(pub [u8; 32]);

impl fmt::Debug for ChildRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChildRef(")?;
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        write!(f, ")")
    }
}

/// Which form a body's child references take.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RefForm {
    /// The tag and the child's raw digest: what typed codecs read and write.
    Logical,
    /// The tag and the child's minimal slot index: what an envelope stores.
    Physical,
}

/// A spelling the grammar admits but a canonical body never holds.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CanonicalFault {
    #[error("a varint is not minimal")]
    NonMinimalVarint,
    #[error("an arbitrary integer is not minimal")]
    NonMinimalInteger,
    #[error("a float holds a NaN other than the canonical one")]
    InvalidNaN,
    #[error("unknown token tag {0:#04x}")]
    UnknownTag(u8),
    #[error("expected token {expected:#04x}, found {found:#04x}")]
    UnexpectedTag { expected: u8, found: u8 },
    #[error("map keys are unsorted or repeated")]
    UnsortedOrDuplicateKey,
    #[error("the child table is unsorted or repeats a child")]
    UnsortedOrDuplicateChild,
    #[error("expected {expected} fields, found {found}")]
    FieldCount { expected: u64, found: u64 },
    #[error("child slot {0} is never referenced")]
    UnusedChildSlot(u64),
    #[error("child slot {0} is outside the child table")]
    ChildSlotOutOfRange(u64),
    #[error("a large value's chunks or index are not in canonical shape")]
    InvalidChunking,
    #[error("an aggregate's declared length is not its contents' length")]
    AggregateLengthMismatch,
    #[error("a text or bytes value is not placed by the inline rule")]
    InvalidPlacement,
    #[error("a body is inlined where it must stream, or streams where it must inline")]
    InvalidBodyForm,
    #[error("a body is not exactly one token")]
    NotOneToken,
}

/// The decode dimension a limit bounds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum DecodeDimension {
    /// One node's envelope bytes.
    NodeBytes,
    /// The bytes of every node one decode reads.
    TotalBytes,
    /// The nodes one decode reads.
    Nodes,
    /// One logical body's bytes.
    LogicalBytes,
    /// Nesting depth.
    Depth,
    /// Elements of one sequence or entries of one map.
    Count,
    /// Work units a decode spends.
    Work,
    /// Heap objects a kernel state holds.
    Objects,
    /// Tasks a kernel state holds.
    Tasks,
    /// Frames of one task.
    Frames,
    /// Arguments of one call.
    Arguments,
}

/// Why a token stream did not decode.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("the body ends inside a token")]
    Truncated,
    #[error("the body has bytes after its last token")]
    TrailingBytes,
    #[error("a count or number overflows its width")]
    LengthOverflow,
    #[error("text is not UTF-8")]
    InvalidUtf8,
    #[error("non-canonical body: {0}")]
    NonCanonical(CanonicalFault),
    #[error("variant {variant} is not one this decoder supports")]
    UnsupportedVariant { variant: u32 },
    #[error("{dimension:?} {actual} exceeds the limit {limit}")]
    Limit {
        dimension: DecodeDimension,
        actual: u64,
        limit: u64,
    },
    /// A typed decoder's semantic refusal of well-formed tokens.
    #[error("invalid value: {0}")]
    Invalid(String),
}

impl From<CanonicalFault> for TokenError {
    fn from(fault: CanonicalFault) -> Self {
        Self::NonCanonical(fault)
    }
}

/// One token, its container headers without their contents.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TokenEvent<'a> {
    Unit,
    Bool(bool),
    Unsigned(u64),
    Signed(i64),
    /// An arbitrary integer: its sign and its minimal little-endian
    /// magnitude.
    BigInteger {
        negative: bool,
        magnitude: &'a [u8],
    },
    Float(f64),
    Text(&'a str),
    Bytes(&'a [u8]),
    Seq(u64),
    Map(u64),
    /// A logical child reference.
    Child(ChildRef),
    /// A physical child reference.
    Slot(u64),
    Sum {
        variant: u32,
        fields: u64,
    },
    None,
    Some,
}

impl TokenEvent<'_> {
    /// How many tokens follow this one as its contents.
    #[must_use]
    pub fn contents(&self) -> u64 {
        match self {
            Self::Seq(count) => *count,
            Self::Map(count) => count.saturating_mul(2),
            Self::Sum { fields, .. } => *fields,
            Self::Some => 1,
            _ => 0,
        }
    }

    /// Whether a block boundary may not split this token: every token but a
    /// container header is a scalar.
    #[must_use]
    pub fn is_header(&self) -> bool {
        matches!(
            self,
            Self::Seq(_) | Self::Map(_) | Self::Sum { .. } | Self::Some
        )
    }
}

/// The number of bytes `value`'s minimal LEB128 takes.
#[must_use]
pub const fn varint_len(value: u64) -> usize {
    let bits = 64 - value.leading_zeros() as usize;
    if bits == 0 { 1 } else { bits.div_ceil(7) }
}

/// Appends `value`'s minimal LEB128.
pub fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let low = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(low);
            return;
        }
        out.push(low | 0x80);
    }
}

/// Reads one minimal LEB128 u64 at `*pos`, refusing an overlong or
/// overflowed spelling.
///
/// # Errors
///
/// [`TokenError::Truncated`], [`TokenError::LengthOverflow`] or a
/// non-minimal spelling.
pub fn get_varint(input: &[u8], pos: &mut usize) -> Result<u64, TokenError> {
    let mut value = 0_u64;
    for index in 0..MAX_VARINT_BYTES {
        let byte = *input.get(*pos + index).ok_or(TokenError::Truncated)?;
        let payload = u64::from(byte & 0x7f);
        if index == MAX_VARINT_BYTES - 1 && byte > 1 {
            return Err(TokenError::LengthOverflow);
        }
        value |= payload << (7 * index);
        if byte & 0x80 == 0 {
            if index > 0 && byte == 0 {
                return Err(CanonicalFault::NonMinimalVarint.into());
            }
            *pos += index + 1;
            return Ok(value);
        }
    }
    Err(TokenError::LengthOverflow)
}

/// Converts a decoded count to a width an index or allocation uses.
///
/// # Errors
///
/// [`TokenError::LengthOverflow`] when it does not fit.
pub fn checked_usize(count: u64) -> Result<usize, TokenError> {
    usize::try_from(count).map_err(|_| TokenError::LengthOverflow)
}

/// Refuses `actual` above `limit` in `dimension`.
///
/// # Errors
///
/// [`TokenError::Limit`].
pub fn within(dimension: DecodeDimension, actual: u64, limit: u64) -> Result<(), TokenError> {
    if actual > limit {
        Err(TokenError::Limit {
            dimension,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

const fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

const fn unzigzag(value: u64) -> i64 {
    ((value >> 1) as i64) ^ -((value & 1) as i64)
}

/// Writes a token stream. It checks no structure: the typed codec writing
/// it owns field counts and ordering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenWriter {
    out: Vec<u8>,
}

impl TokenWriter {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            out: Vec::with_capacity(capacity),
        }
    }

    fn tag(&mut self, tag: Tag) {
        self.out.push(tag.byte());
    }

    pub fn unit(&mut self) {
        self.tag(Tag::Unit);
    }

    pub fn bool(&mut self, value: bool) {
        self.tag(if value { Tag::True } else { Tag::False });
    }

    pub fn unsigned(&mut self, value: u64) {
        self.tag(Tag::Unsigned);
        put_varint(&mut self.out, value);
    }

    pub fn signed(&mut self, value: i64) {
        self.tag(Tag::Signed);
        put_varint(&mut self.out, zigzag(value));
    }

    /// An arbitrary integer from its sign and little-endian magnitude. High
    /// zero bytes are dropped and zero is written unsigned, so the spelling
    /// is the canonical one whatever the caller passed.
    pub fn big_integer(&mut self, negative: bool, magnitude_le: &[u8]) {
        let used = magnitude_le
            .iter()
            .rposition(|byte| *byte != 0)
            .map_or(0, |last| last + 1);
        let magnitude = &magnitude_le[..used];
        self.tag(Tag::BigInteger);
        self.out.push(u8::from(negative && used > 0));
        put_varint(&mut self.out, magnitude.len() as u64);
        self.out.extend_from_slice(magnitude);
    }

    /// A float; every NaN is written as the canonical one. Signed zero is
    /// kept.
    pub fn float(&mut self, value: f64) {
        let bits = if value.is_nan() {
            CANONICAL_NAN_BITS
        } else {
            value.to_bits()
        };
        self.tag(Tag::Float);
        self.out.extend_from_slice(&bits.to_le_bytes());
    }

    pub fn text(&mut self, value: &str) {
        self.tag(Tag::Text);
        put_varint(&mut self.out, value.len() as u64);
        self.out.extend_from_slice(value.as_bytes());
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.tag(Tag::Bytes);
        put_varint(&mut self.out, value.len() as u64);
        self.out.extend_from_slice(value);
    }

    /// A sequence, tuple or struct header; `count` tokens follow.
    pub fn seq(&mut self, count: u64) {
        self.tag(Tag::Seq);
        put_varint(&mut self.out, count);
    }

    /// An unordered map header; `count` key, value pairs follow in the
    /// schema's key order.
    pub fn map(&mut self, count: u64) {
        self.tag(Tag::Map);
        put_varint(&mut self.out, count);
    }

    /// A logical child reference.
    pub fn child(&mut self, child: ChildRef) {
        self.tag(Tag::Ref);
        self.out.extend_from_slice(&child.0);
    }

    /// A physical child reference: the graph layer's, never a typed codec's.
    pub fn slot(&mut self, slot: u64) {
        self.tag(Tag::Ref);
        put_varint(&mut self.out, slot);
    }

    /// A sum header; `fields` tokens follow.
    pub fn sum(&mut self, variant: u32, fields: u64) {
        self.tag(Tag::Sum);
        put_varint(&mut self.out, u64::from(variant));
        put_varint(&mut self.out, fields);
    }

    pub fn none(&mut self) {
        self.tag(Tag::None);
    }

    /// The `some` header; one token follows.
    pub fn some(&mut self) {
        self.tag(Tag::Some);
    }

    /// Writes one token as an event spells it.
    pub fn event(&mut self, event: &TokenEvent<'_>) {
        match *event {
            TokenEvent::Unit => self.unit(),
            TokenEvent::Bool(value) => self.bool(value),
            TokenEvent::Unsigned(value) => self.unsigned(value),
            TokenEvent::Signed(value) => self.signed(value),
            TokenEvent::BigInteger {
                negative,
                magnitude,
            } => self.big_integer(negative, magnitude),
            TokenEvent::Float(value) => self.float(value),
            TokenEvent::Text(value) => self.text(value),
            TokenEvent::Bytes(value) => self.bytes(value),
            TokenEvent::Seq(count) => self.seq(count),
            TokenEvent::Map(count) => self.map(count),
            TokenEvent::Child(child) => self.child(child),
            TokenEvent::Slot(slot) => self.slot(slot),
            TokenEvent::Sum { variant, fields } => self.sum(variant, fields),
            TokenEvent::None => self.none(),
            TokenEvent::Some => self.some(),
        }
    }

    /// Appends tokens another writer wrote, as they are.
    pub fn raw(&mut self, tokens: &[u8]) {
        self.out.extend_from_slice(tokens);
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.out.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.out.is_empty()
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.out
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.out
    }
}

/// Reads a token stream, refusing every non-canonical spelling.
#[derive(Clone, Debug)]
pub struct TokenReader<'a> {
    limits: crate::KernelDataLimits,
    input: &'a [u8],
    pos: usize,
    form: RefForm,
}

impl<'a> TokenReader<'a> {
    /// A reader over a logical body: references are digests.
    #[must_use]
    pub fn new(input: &'a [u8]) -> Self {
        Self::with_form(input, RefForm::Logical)
    }

    #[must_use]
    pub fn with_form(input: &'a [u8], form: RefForm) -> Self {
        Self {
            limits: crate::KernelDataLimits::standard(),
            input,
            pos: 0,
            form,
        }
    }

    pub fn with_data_limits(mut self, limits: crate::KernelDataLimits) -> Self {
        self.limits = limits;
        self
    }
    pub fn decode_depth(&self) -> u64 {
        self.limits.decode_depth()
    }

    /// Whether every byte was read.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.pos == self.input.len()
    }

    /// The offset of the next token.
    #[must_use]
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Refuses unread bytes.
    ///
    /// # Errors
    ///
    /// [`TokenError::TrailingBytes`].
    pub fn finish(self) -> Result<(), TokenError> {
        if self.is_empty() {
            Ok(())
        } else {
            Err(TokenError::TrailingBytes)
        }
    }

    fn take(&mut self, len: u64) -> Result<&'a [u8], TokenError> {
        let len = checked_usize(len)?;
        let end = self
            .pos
            .checked_add(len)
            .ok_or(TokenError::LengthOverflow)?;
        let slice = self.input.get(self.pos..end).ok_or(TokenError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    fn varint(&mut self) -> Result<u64, TokenError> {
        get_varint(self.input, &mut self.pos)
    }

    /// The next token's tag, unread.
    ///
    /// # Errors
    ///
    /// [`TokenError::Truncated`] at the end, or an unknown tag.
    pub fn peek_tag(&self) -> Result<Tag, TokenError> {
        let byte = *self.input.get(self.pos).ok_or(TokenError::Truncated)?;
        Tag::from_byte(byte).ok_or(TokenError::NonCanonical(CanonicalFault::UnknownTag(byte)))
    }

    /// Reads the next token, whatever it is.
    ///
    /// # Errors
    ///
    /// Any refusal of the grammar.
    pub fn next_event(&mut self) -> Result<TokenEvent<'a>, TokenError> {
        let tag = self.peek_tag()?;
        self.pos += 1;
        Ok(match tag {
            Tag::Unit => TokenEvent::Unit,
            Tag::False => TokenEvent::Bool(false),
            Tag::True => TokenEvent::Bool(true),
            Tag::Unsigned => TokenEvent::Unsigned(self.varint()?),
            Tag::Signed => TokenEvent::Signed(unzigzag(self.varint()?)),
            Tag::BigInteger => {
                let sign = self.take(1)?[0];
                let count = self.varint()?;
                let magnitude = self.take(count)?;
                if sign > 1 || magnitude.last() == Some(&0) || (sign == 1 && magnitude.is_empty()) {
                    return Err(CanonicalFault::NonMinimalInteger.into());
                }
                TokenEvent::BigInteger {
                    negative: sign == 1,
                    magnitude,
                }
            }
            Tag::Float => {
                let mut bits = [0_u8; 8];
                bits.copy_from_slice(self.take(8)?);
                let bits = u64::from_le_bytes(bits);
                let value = f64::from_bits(bits);
                if value.is_nan() && bits != CANONICAL_NAN_BITS {
                    return Err(CanonicalFault::InvalidNaN.into());
                }
                TokenEvent::Float(value)
            }
            Tag::Text => {
                let len = self.varint()?;
                let bytes = self.take(len)?;
                TokenEvent::Text(std::str::from_utf8(bytes).map_err(|_| TokenError::InvalidUtf8)?)
            }
            Tag::Bytes => {
                let len = self.varint()?;
                TokenEvent::Bytes(self.take(len)?)
            }
            Tag::Seq => TokenEvent::Seq(self.varint()?),
            Tag::Map => TokenEvent::Map(self.varint()?),
            Tag::Ref => match self.form {
                RefForm::Logical => {
                    let mut digest = [0_u8; 32];
                    digest.copy_from_slice(self.take(32)?);
                    TokenEvent::Child(ChildRef(digest))
                }
                RefForm::Physical => TokenEvent::Slot(self.varint()?),
            },
            Tag::Sum => {
                let variant =
                    u32::try_from(self.varint()?).map_err(|_| TokenError::LengthOverflow)?;
                TokenEvent::Sum {
                    variant,
                    fields: self.varint()?,
                }
            }
            Tag::None => TokenEvent::None,
            Tag::Some => TokenEvent::Some,
        })
    }

    /// Skips one whole token tree, returning how many tokens it held.
    ///
    /// # Errors
    ///
    /// Any refusal of the grammar.
    pub fn skip_tree(&mut self) -> Result<u64, TokenError> {
        let mut pending = 1_u64;
        let mut tokens = 0_u64;
        while pending > 0 {
            let event = self.next_event()?;
            pending = pending - 1 + event.contents();
            tokens += 1;
        }
        Ok(tokens)
    }

    fn expect(&mut self, tag: Tag) -> Result<(), TokenError> {
        let found = self.peek_tag()?;
        if found != tag {
            return Err(CanonicalFault::UnexpectedTag {
                expected: tag.byte(),
                found: found.byte(),
            }
            .into());
        }
        Ok(())
    }

    /// # Errors
    ///
    /// Any other token.
    pub fn unit(&mut self) -> Result<(), TokenError> {
        self.expect(Tag::Unit)?;
        self.pos += 1;
        Ok(())
    }

    /// # Errors
    ///
    /// Any other token.
    pub fn bool(&mut self) -> Result<bool, TokenError> {
        match self.peek_tag()? {
            Tag::False | Tag::True => match self.next_event()? {
                TokenEvent::Bool(value) => Ok(value),
                _ => Err(TokenError::Truncated),
            },
            other => Err(CanonicalFault::UnexpectedTag {
                expected: Tag::False.byte(),
                found: other.byte(),
            }
            .into()),
        }
    }

    /// # Errors
    ///
    /// Any other token.
    pub fn unsigned(&mut self) -> Result<u64, TokenError> {
        self.expect(Tag::Unsigned)?;
        self.pos += 1;
        self.varint()
    }

    /// An unsigned integer that must fit a u32.
    ///
    /// # Errors
    ///
    /// Any other token, or [`TokenError::LengthOverflow`].
    pub fn unsigned_u32(&mut self) -> Result<u32, TokenError> {
        u32::try_from(self.unsigned()?).map_err(|_| TokenError::LengthOverflow)
    }

    /// # Errors
    ///
    /// Any other token.
    pub fn signed(&mut self) -> Result<i64, TokenError> {
        self.expect(Tag::Signed)?;
        self.pos += 1;
        Ok(unzigzag(self.varint()?))
    }

    /// An arbitrary integer: its sign and minimal little-endian magnitude.
    ///
    /// # Errors
    ///
    /// Any other token, or a non-minimal spelling.
    pub fn big_integer(&mut self) -> Result<(bool, &'a [u8]), TokenError> {
        self.expect(Tag::BigInteger)?;
        match self.next_event()? {
            TokenEvent::BigInteger {
                negative,
                magnitude,
            } => Ok((negative, magnitude)),
            _ => Err(TokenError::Truncated),
        }
    }

    /// # Errors
    ///
    /// Any other token, or a non-canonical NaN.
    pub fn float(&mut self) -> Result<f64, TokenError> {
        self.expect(Tag::Float)?;
        match self.next_event()? {
            TokenEvent::Float(value) => Ok(value),
            _ => Err(TokenError::Truncated),
        }
    }

    /// # Errors
    ///
    /// Any other token, or bytes that are not UTF-8.
    pub fn text(&mut self) -> Result<&'a str, TokenError> {
        self.expect(Tag::Text)?;
        match self.next_event()? {
            TokenEvent::Text(value) => Ok(value),
            _ => Err(TokenError::Truncated),
        }
    }

    /// # Errors
    ///
    /// Any other token.
    pub fn bytes(&mut self) -> Result<&'a [u8], TokenError> {
        self.expect(Tag::Bytes)?;
        match self.next_event()? {
            TokenEvent::Bytes(value) => Ok(value),
            _ => Err(TokenError::Truncated),
        }
    }

    /// A sequence header: its element count.
    ///
    /// # Errors
    ///
    /// Any other token.
    pub fn seq(&mut self) -> Result<u64, TokenError> {
        self.expect(Tag::Seq)?;
        self.pos += 1;
        self.varint()
    }

    /// A struct header with exactly `fields` fields.
    ///
    /// # Errors
    ///
    /// Any other token, or another field count.
    pub fn expect_struct(&mut self, fields: u64) -> Result<(), TokenError> {
        let found = self.seq()?;
        if found != fields {
            return Err(CanonicalFault::FieldCount {
                expected: fields,
                found,
            }
            .into());
        }
        Ok(())
    }

    /// A map header: its entry count.
    ///
    /// # Errors
    ///
    /// Any other token.
    pub fn map(&mut self) -> Result<u64, TokenError> {
        self.expect(Tag::Map)?;
        self.pos += 1;
        self.varint()
    }

    /// A logical child reference.
    ///
    /// # Errors
    ///
    /// Any other token, or a physical reader.
    pub fn child(&mut self) -> Result<ChildRef, TokenError> {
        self.expect(Tag::Ref)?;
        match self.next_event()? {
            TokenEvent::Child(child) => Ok(child),
            _ => Err(CanonicalFault::UnexpectedTag {
                expected: Tag::Ref.byte(),
                found: Tag::Ref.byte(),
            }
            .into()),
        }
    }

    /// A sum header: its variant and field count.
    ///
    /// # Errors
    ///
    /// Any other token.
    pub fn sum(&mut self) -> Result<(u32, u64), TokenError> {
        self.expect(Tag::Sum)?;
        match self.next_event()? {
            TokenEvent::Sum { variant, fields } => Ok((variant, fields)),
            _ => Err(TokenError::Truncated),
        }
    }

    /// A sum variant's field count, checked against the variant's arity.
    ///
    /// # Errors
    ///
    /// Another field count.
    pub fn expect_fields(found: u64, expected: u64) -> Result<(), TokenError> {
        if found == expected {
            Ok(())
        } else {
            Err(CanonicalFault::FieldCount { expected, found }.into())
        }
    }

    /// An option header: `true` when `some` and one token follows.
    ///
    /// # Errors
    ///
    /// Any other token.
    pub fn option(&mut self) -> Result<bool, TokenError> {
        match self.peek_tag()? {
            Tag::None => {
                self.pos += 1;
                Ok(false)
            }
            Tag::Some => {
                self.pos += 1;
                Ok(true)
            }
            other => Err(CanonicalFault::UnexpectedTag {
                expected: Tag::None.byte(),
                found: other.byte(),
            }
            .into()),
        }
    }
}

/// A type with one canonical token encoding.
///
/// `encode` writes exactly one token tree and `decode` reads exactly one,
/// refusing every other spelling: decoding what `encode` wrote yields an
/// equal value, and encoding a decoded value yields the bytes it was decoded
/// from.
pub trait TokenCodec: Sized {
    fn encode(&self, out: &mut TokenWriter);

    /// # Errors
    ///
    /// Any token the canonical encoding of `Self` does not hold.
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError>;
}

/// Encodes `value` as one logical body.
#[must_use]
pub fn encode_body<T: TokenCodec>(value: &T) -> Vec<u8> {
    let mut out = TokenWriter::new();
    value.encode(&mut out);
    out.into_bytes()
}

/// Decodes one logical body that holds exactly one `T`.
///
/// # Errors
///
/// Any refusal of `T`'s decoder, or trailing bytes.
pub fn decode_body<T: TokenCodec>(body: &[u8]) -> Result<T, TokenError> {
    decode_body_with_limits(body, crate::KernelDataLimits::standard())
}
/// Decode a logical body under deployment recursion safety policy.
pub fn decode_body_with_limits<T: TokenCodec>(
    body: &[u8],
    limits: crate::KernelDataLimits,
) -> Result<T, TokenError> {
    let mut input = TokenReader::new(body).with_data_limits(limits);
    let value = T::decode(&mut input)?;
    input.finish()?;
    Ok(value)
}

impl TokenCodec for () {
    fn encode(&self, out: &mut TokenWriter) {
        out.unit();
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unit()
    }
}

impl TokenCodec for bool {
    fn encode(&self, out: &mut TokenWriter) {
        out.bool(*self);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.bool()
    }
}

impl TokenCodec for u64 {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(*self);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unsigned()
    }
}

impl TokenCodec for u32 {
    fn encode(&self, out: &mut TokenWriter) {
        out.unsigned(u64::from(*self));
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.unsigned_u32()
    }
}

impl TokenCodec for i64 {
    fn encode(&self, out: &mut TokenWriter) {
        out.signed(*self);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.signed()
    }
}

impl TokenCodec for f64 {
    fn encode(&self, out: &mut TokenWriter) {
        out.float(*self);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.float()
    }
}

impl TokenCodec for String {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.text().map(str::to_owned)
    }
}

impl TokenCodec for ChildRef {
    fn encode(&self, out: &mut TokenWriter) {
        out.child(*self);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.child()
    }
}

impl<T: TokenCodec> TokenCodec for Option<T> {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            None => out.none(),
            Some(value) => {
                out.some();
                value.encode(out);
            }
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        if input.option()? {
            T::decode(input).map(Some)
        } else {
            Ok(None)
        }
    }
}

/// A sequence in its own order. Unordered collections sort before they
/// encode; this impl keeps whatever order it is given.
impl<T: TokenCodec> TokenCodec for Vec<T> {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(self.len() as u64);
        for item in self {
            item.encode(out);
        }
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let count = input.seq()?;
        // Each element takes at least one byte: a count beyond the unread
        // bytes is refused before it sizes an allocation.
        let remaining = (input.input.len() - input.pos) as u64;
        if count > remaining {
            return Err(TokenError::Truncated);
        }
        let mut items = Vec::with_capacity(checked_usize(count)?);
        for _ in 0..count {
            items.push(T::decode(input)?);
        }
        Ok(items)
    }
}

/// Checks that `keys`, already decoded in stream order, ascend strictly.
///
/// # Errors
///
/// [`CanonicalFault::UnsortedOrDuplicateKey`].
pub fn check_ascending<K: Ord>(keys: &[K]) -> Result<(), TokenError> {
    if keys.windows(2).all(|pair| pair[0] < pair[1]) {
        Ok(())
    } else {
        Err(CanonicalFault::UnsortedOrDuplicateKey.into())
    }
}

/// An enum's explicit codec variant number (FIG-5887 §2).
///
/// Every number is written out in an exhaustive match: a new Rust variant
/// fails to compile until it is given an unused number, and reordering the
/// declaration changes nothing. Numbers are never reused or renumbered.
pub trait CodecVariant {
    /// This value's variant number.
    fn codec_variant(&self) -> u32;
}

#[cfg(test)]
#[path = "graph_tokens_tests.rs"]
mod tests;
