//! Canonical package surface names, independent of dialect syntax.

use std::{fmt, ops::Deref};

use serde::{Deserialize, Serialize};

/// A surface name matching `[a-z][a-z0-9]*(_[a-z][a-z0-9]*)*`.
/// Definition names and record field names do not use this grammar.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Segment(String);

/// A name that is not a canonical surface segment.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not a canonical segment: expected [a-z][a-z0-9]*(_[a-z][a-z0-9]*)*")]
pub struct InvalidSegment {
    pub text: String,
}

impl Segment {
    pub fn new(text: impl Into<String>) -> Result<Self, InvalidSegment> {
        let text = text.into();
        if Self::is_canonical(&text) {
            Ok(Self(text))
        } else {
            Err(InvalidSegment { text })
        }
    }

    /// Whether a single segment has the canonical grammar.
    pub fn is_canonical(text: &str) -> bool {
        text.split('_').all(|word| {
            let mut bytes = word.bytes();
            bytes.next().is_some_and(|first| first.is_ascii_lowercase())
                && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl TryFrom<String> for Segment {
    type Error = InvalidSegment;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl TryFrom<&str> for Segment {
    type Error = InvalidSegment;

    fn try_from(text: &str) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<Segment> for String {
    fn from(segment: Segment) -> Self {
        segment.0
    }
}

/// A vector with at least one element. Its slice view cannot change its length.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Vec1<T>(Vec<T>);

/// An empty vector cannot name a package path.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("expected at least one element")]
pub struct EmptyPath;

impl<T> Vec1<T> {
    pub fn new(first: T) -> Self {
        Self(vec![first])
    }

    pub fn push(&mut self, value: T) {
        self.0.push(value);
    }

    pub fn as_slice(&self) -> &[T] {
        &self.0
    }

    pub fn into_vec(self) -> Vec<T> {
        self.0
    }
}

impl<T> TryFrom<Vec<T>> for Vec1<T> {
    type Error = EmptyPath;

    fn try_from(values: Vec<T>) -> Result<Self, Self::Error> {
        if values.is_empty() {
            Err(EmptyPath)
        } else {
            Ok(Self(values))
        }
    }
}

impl<T> Deref for Vec1<T> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl<T> IntoIterator for Vec1<T> {
    type Item = T;
    type IntoIter = std::vec::IntoIter<T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a, T> IntoIterator for &'a Vec1<T> {
    type Item = &'a T;
    type IntoIter = std::slice::Iter<'a, T>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// The nonempty, ordered canonical segments of a package namespace.
/// Serialized as its canonical dotted path, including when used as a map key.
pub type PackagePath = Vec1<Segment>;

impl PackagePath {
    pub fn parse(text: &str) -> Result<Self, InvalidSegment> {
        let mut segments = text.split('.');
        let mut path = Self::new(Segment::new(segments.next().unwrap_or_default())?);
        for segment in segments {
            path.push(Segment::new(segment)?);
        }
        Ok(path)
    }
}

impl fmt::Display for PackagePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, segment) in self.iter().enumerate() {
            if index > 0 {
                f.write_str(".")?;
            }
            segment.fmt(f)?;
        }
        Ok(())
    }
}

impl Serialize for PackagePath {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PackagePath {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

/// An export, or one member of an exported value type.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct MemberPath {
    pub export: Segment,
    pub member: Option<Segment>,
}

impl fmt::Display for MemberPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.export.fmt(f)?;
        if let Some(member) = &self.member {
            write!(f, ".{member}")?;
        }
        Ok(())
    }
}

impl TryFrom<String> for MemberPath {
    type Error = InvalidSegment;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        let (export, member) = text
            .split_once('.')
            .map_or((text.as_str(), None), |(export, member)| {
                (export, Some(member))
            });
        Ok(Self {
            export: Segment::new(export)?,
            member: member.map(Segment::new).transpose()?,
        })
    }
}

impl From<MemberPath> for String {
    fn from(path: MemberPath) -> Self {
        path.to_string()
    }
}

/// An operation has exactly one name segment beneath a nonempty package path.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct QualifiedOperation {
    pub package: PackagePath,
    pub name: Segment,
}

/// A kernel effect name outside the canonical package-operation namespace.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{effect}` is not a canonical operation: expected a package path and one name segment")]
pub struct InvalidOperation {
    pub effect: EffectName,
}

impl QualifiedOperation {
    pub fn effect_name(&self) -> EffectName {
        let mut text = String::new();
        for segment in &self.package {
            text.push_str(segment.as_str());
            text.push('.');
        }
        text.push_str(self.name.as_str());
        // Segment and Vec1 guarantee this is a nonempty qualified identifier.
        match EffectName::new(text) {
            Ok(effect) => effect,
            Err(_) => unreachable!("canonical operation segments are qualified identifiers"),
        }
    }

    /// Split at the last dot, recovering an operation from its effect alone.
    /// Kernel effect names also admit unqualified and noncanonical identifiers;
    /// those are refused at this surface boundary.
    pub fn from_effect(effect: &EffectName) -> Result<Self, InvalidOperation> {
        let invalid = || InvalidOperation {
            effect: effect.clone(),
        };
        let (package, name) = effect.as_str().rsplit_once('.').ok_or_else(invalid)?;
        Ok(Self {
            package: PackagePath::parse(package).map_err(|_| invalid())?,
            name: Segment::new(name).map_err(|_| invalid())?,
        })
    }
}

// The serialized forms are canonical dotted strings, including PackagePath's
// specialized Vec1 serialization. Keep their schemas at the name owner.
macro_rules! name_schema {
    ($ty:ty, $name:literal, $pattern:expr) => {
        impl schemars::JsonSchema for $ty {
            fn schema_name() -> std::borrow::Cow<'static, str> { $name.into() }
            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "pattern": $pattern })
            }
        }
    };
}
const SEGMENT_PATTERN: &str = "[a-z][a-z0-9]*(_[a-z][a-z0-9]*)*";
name_schema!(Segment, "Segment", format!("^{SEGMENT_PATTERN}$"));
name_schema!(
    PackagePath,
    "PackagePath",
    format!("^{SEGMENT_PATTERN}(\\.{SEGMENT_PATTERN})*$")
);
name_schema!(
    MemberPath,
    "MemberPath",
    format!("^{SEGMENT_PATTERN}(\\.{SEGMENT_PATTERN})?$")
);

use crate::{LinkId, Signature};
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(transparent)]
/// A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
pub struct ImportSlot(pub u32);
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// A document Export, optionally a member of its TypeDef. Type members are not start selectors.
pub struct ExportName {
    pub name: Name,
    pub member: Option<Segment>,
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// An Import name parsed by segment, optionally addressing a type member.
pub struct QualifiedName {
    pub package: PackagePath,
    pub member: Segment,
    pub type_member: Option<Segment>,
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// Nominal type or error identity: its defining package and canonical name, independent of representation.
pub struct Brand {
    pub package: PackagePath,
    pub name: Segment,
}
pub type ErrorKind = Brand;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Semantic call convention. Compatibility compares this variant, not another document's helper IDs.
pub enum Convention {
    Direct,
    TsDirect,
    TsArgsList,
    PyDirect,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// A named requirement on external code, an operation, a type, or an error.
pub struct Import {
    pub name: QualifiedName,
    pub kind: ImportKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// The compatibility facts required to fill an Import. Natives and cell variables are not Imports.
pub enum ImportKind {
    Operation {
        signature: Signature,
    },
    Code {
        signature: Signature,
        convention: Convention,
    },
    Type {
        brand: Brand,
    },
    Error {
        kind: ErrorKind,
    },
}
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// One operation Import in one immutable Link. All dispatch, modes and pins use this identity.
pub struct OperationUse {
    pub link: LinkId,
    pub slot: ImportSlot,
}
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// A stored function under its defining Link. Calls and escaped captures retain this Link.
pub struct CallableId {
    pub link: LinkId,
    pub function: FunctionId,
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Main requires a main block; Export names a top-level function and never runs main.
pub enum StartSelector {
    Main,
    Export(Name),
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// The immutable Link and selected start that determine process definition identity.
pub struct LinkedStart {
    pub link: LinkId,
    pub selector: StartSelector,
}

impl std::fmt::Display for Brand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.package, self.name)
    }
}
impl std::fmt::Display for QualifiedName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.package, self.member)?;
        if let Some(member) = &self.type_member {
            write!(f, ".{member}")?;
        }
        Ok(())
    }
}

impl std::fmt::Display for OperationUse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.link, self.slot.0)
    }
}

use std::sync::Arc;

use schemars::JsonSchema;

/// Implements `JsonSchema` for a type stored as a string of one pattern.
#[macro_export]
macro_rules! string_schema {
    ($type:ty, $name:expr, $pattern:literal) => {
        impl schemars::JsonSchema for $type {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                $name.into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "pattern": $pattern })
            }
        }
    };
}
#[doc(hidden)]
pub use string_schema;

/// A variable, parameter or declared-function name.
///
/// Any text is a name; the kernel gives none of them meaning. Structural
/// validation refuses the empty name.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct Name(Arc<str>);

impl Name {
    pub fn new(name: impl Into<String>) -> Self {
        Self(Arc::from(name.into()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The name a session saves its `n`th function held by no binding of
    /// its own under: a closure reached only through carried data or a
    /// capture (SAVE-KEY). Its form, `saved#n`, is an identifier in no
    /// dialect, so no source declares it, and every printer renders it as
    /// a generated name.
    #[must_use]
    pub fn saved(n: u64) -> Self {
        Self::new(format!("{SAVED_PREFIX}{n}"))
    }

    /// `n` when this is [`Name::saved`]`(n)`.
    #[must_use]
    pub fn saved_index(&self) -> Option<u64> {
        let digits = self.0.strip_prefix(SAVED_PREFIX)?;
        let n: u64 = digits.parse().ok()?;
        (n.to_string() == digits).then_some(n)
    }
}

/// What every [`Name::saved`] starts with.
const SAVED_PREFIX: &str = "saved#";

impl fmt::Display for Name {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<&str> for Name {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

/// A text that is not a qualified name.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not a qualified name: write identifiers joined by `.`")]
pub struct InvalidQualifiedName {
    pub text: String,
}

/// The name of a library function or an effect: one or more identifiers
/// (`[A-Za-z_][A-Za-z0-9_]*`) joined by `.`, as in `text.len` or
/// `regex.ecma.exec`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct FunctionName(String);

string_schema!(
    FunctionName,
    "FunctionName",
    r"^[A-Za-z_][A-Za-z0-9_]*(\.[A-Za-z_][A-Za-z0-9_]*)*$"
);

impl FunctionName {
    /// A name read from where a qualified name was written, unchecked.
    #[doc(hidden)]
    pub fn written(text: &str) -> Self {
        Self(text.to_owned())
    }

    pub fn new(name: impl Into<String>) -> Result<Self, InvalidQualifiedName> {
        let text = name.into();
        if text.split('.').all(is_identifier) {
            Ok(Self(text))
        } else {
            Err(InvalidQualifiedName { text })
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// The identifiers the name is made of, in order.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('.')
    }
}

impl fmt::Display for FunctionName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for FunctionName {
    type Error = InvalidQualifiedName;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::new(text)
    }
}

impl From<FunctionName> for String {
    fn from(name: FunctionName) -> Self {
        name.0
    }
}

/// The name of a library function.

/// The name of an effect a host supplies.
pub type EffectName = FunctionName;

/// Whether `text` is one identifier: `[A-Za-z_][A-Za-z0-9_]*`.
#[doc(hidden)]
pub fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A text that is not a 32-byte digest in lower-case hexadecimal.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("`{text}` is not an identity: write 64 lower-case hexadecimal digits")]
pub struct InvalidIdentity {
    pub text: String,
}

fn parse_digest(text: &str) -> Result<[u8; 32], InvalidIdentity> {
    let invalid = || InvalidIdentity {
        text: text.to_string(),
    };
    let bytes = text.as_bytes();
    if bytes.len() != 64 {
        return Err(invalid());
    }
    let mut digest = [0u8; 32];
    for (byte, pair) in digest.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let high = hex_value(pair[0]).ok_or_else(invalid)?;
        let low = hex_value(pair[1]).ok_or_else(invalid)?;
        *byte = (high << 4) | low;
    }
    Ok(digest)
}

fn hex_value(digit: u8) -> Option<u8> {
    match digit {
        b'0'..=b'9' => Some(digit - b'0'),
        b'a'..=b'f' => Some(digit - b'a' + 10),
        _ => None,
    }
}

#[doc(hidden)]
pub fn write_hex(bytes: &[u8], out: &mut String) {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
}

#[doc(hidden)]
pub fn parse_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    let (pairs, rest) = bytes.as_chunks::<2>();
    if !rest.is_empty() {
        return None;
    }
    pairs
        .iter()
        .map(|pair| Some((hex_value(pair[0])? << 4) | hex_value(pair[1])?))
        .collect()
}

macro_rules! identity {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(try_from = "String", into = "String")]
        pub struct $name([u8; 32]);

        // Byte order, compared a word at a time rather than through a
        // library call: registries and indexes compare identities often.
        impl Ord for $name {
            fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                let words = |digest: &[u8; 32]| {
                    let (words, _) = digest.as_chunks::<8>();
                    [
                        u64::from_be_bytes(words[0]),
                        u64::from_be_bytes(words[1]),
                        u64::from_be_bytes(words[2]),
                        u64::from_be_bytes(words[3]),
                    ]
                };
                words(&self.0).cmp(&words(&other.0))
            }
        }

        impl PartialOrd for $name {
            fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                Some(self.cmp(other))
            }
        }

        impl JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({ "type": "string", "pattern": "^[0-9a-f]{64}$" })
            }
        }

        impl $name {
            pub const fn from_bytes(digest: [u8; 32]) -> Self {
                Self(digest)
            }

            pub fn as_bytes(&self) -> &[u8; 32] {
                &self.0
            }

            /// Reads 64 lower-case hexadecimal digits.
            pub fn parse(text: &str) -> Result<Self, InvalidIdentity> {
                parse_digest(text).map(Self)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                let mut text = String::with_capacity(64);
                write_hex(&self.0, &mut text);
                f.write_str(&text)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({self})", stringify!($name))
            }
        }

        impl TryFrom<String> for $name {
            type Error = InvalidIdentity;

            fn try_from(text: String) -> Result<Self, Self::Error> {
                Self::parse(&text)
            }
        }

        impl From<$name> for String {
            fn from(identity: $name) -> Self {
                identity.to_string()
            }
        }
    };
}

identity!(
    /// The content-addressed identity of a library function: the logical
    /// digest of its definition's canonical token body (`K-ID-002`).
    FunctionId
);
identity!(
    /// A document's behavioural identity: the logical digest of its
    /// canonical token body, which holds no source map (`K-ID-001`).
    DocumentId
);
identity!(
    /// A source map's identity: the digest of its canonical encoding, which
    /// names its document and the SHA-256 of its source.
    SourceMapId
);
