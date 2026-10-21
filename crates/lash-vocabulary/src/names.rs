//! Canonical package surface names, independent of dialect syntax.

use std::{fmt, ops::Deref};

use crate::legacy_names::EffectName;
pub use crate::legacy_names::{DocumentId, FunctionId, FunctionName, Name};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
/// A stable document-local Import coordinate. Names are diagnostic, never dispatch keys.
pub struct ImportSlot(pub u32);
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A document Export, optionally a member of its TypeDef. Type members are not start selectors.
pub struct ExportName {
    pub name: Name,
    pub member: Option<Segment>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// An Import name parsed by segment, optionally addressing a type member.
pub struct QualifiedName {
    pub package: PackagePath,
    pub member: Segment,
    pub type_member: Option<Segment>,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Nominal type or error identity: its defining package and canonical name, independent of representation.
pub struct Brand {
    pub package: PackagePath,
    pub name: Segment,
}
pub type ErrorKind = Brand;
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A named requirement on external code, an operation, a type, or an error.
pub struct Import {
    pub name: QualifiedName,
    pub kind: ImportKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// One operation Import in one immutable Link. All dispatch, modes and pins use this identity.
pub struct OperationUse {
    pub link: LinkId,
    pub slot: ImportSlot,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A stored function under its defining Link. Calls and escaped captures retain this Link.
pub struct CallableId {
    pub link: LinkId,
    pub function: FunctionId,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
