//! What a caption names: a site of the document's code, a declared
//! function's parameter, or a named field of a type an entry's parameter or
//! result has (`K-CAP-007`).
//!
//! A parameter and a field are keyed by name, never by position or by the
//! statements around them: an edit that moves statements leaves the key as
//! it was, and a parameter or field that is renamed or removed no longer
//! resolves, so its caption is dropped rather than moved onto a neighbour.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{Caption, CaptionKind};
use crate::{Document, Name, Signature, Site, Type, Unit};

/// What one caption names. Captions order by target: sites first, then
/// parameters, then fields.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CaptionTarget {
    /// A statement, a clause arm or a declared function's body.
    Site(Site),
    /// A parameter of a declared function.
    Param(ParamTarget),
    /// A named field of an entry's parameter or result type.
    Field(FieldTarget),
}

/// A declared function's parameter, by name. An entry's parameters are
/// its signature's, the names a host starts it with; another function's are
/// its own.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct ParamTarget {
    pub function: Name,
    pub param: Name,
}

/// A named field of a type an entry's signature states.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct FieldTarget {
    /// The entry.
    pub function: Name,
    pub path: FieldPath,
}

/// A walk from a parameter's or the result's type to a named record field.
/// Each step names a field of a record. A list is walked to its element
/// type, and a union whose members other than null and absent are exactly
/// one is walked to that member, so `items.title` names the `title` of each
/// record of a list `items`, and `reply.note` the `note` of an optional
/// `reply`. A map, a set, a tuple, a record's rest and a union of two
/// records are not walked.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct FieldPath {
    pub root: FieldRoot,
    /// At least one field name.
    #[schemars(length(min = 1))]
    pub fields: Vec<String>,
}

/// Where a [`FieldPath`] starts.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum FieldRoot {
    /// The type of the entry's parameter of this name.
    Param(Name),
    /// The entry's result type.
    Result,
}

impl std::fmt::Display for CaptionTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Site(site) => write!(f, "site {site}"),
            Self::Param(param) => write!(f, "parameter {} of fn {}", param.param, param.function),
            Self::Field(field) => write!(f, "field {} of fn {}", field.path, field.function),
        }
    }
}

/// `event.answer.choice`; a result's path starts at `return`, which no
/// parameter is named.
impl std::fmt::Display for FieldPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.root {
            FieldRoot::Param(name) => write!(f, "{name}")?,
            FieldRoot::Result => f.write_str("return")?,
        }
        for field in &self.fields {
            write!(f, ".{field}")?;
        }
        Ok(())
    }
}

impl From<Site> for CaptionTarget {
    fn from(site: Site) -> Self {
        Self::Site(site)
    }
}

impl From<ParamTarget> for CaptionTarget {
    fn from(param: ParamTarget) -> Self {
        Self::Param(param)
    }
}

impl From<FieldTarget> for CaptionTarget {
    fn from(field: FieldTarget) -> Self {
        Self::Field(field)
    }
}

impl CaptionTarget {
    /// What the target is in `document`, when a caption may name it.
    #[must_use]
    pub fn kind(&self, document: &Document) -> Option<CaptionKind> {
        match self {
            Self::Site(site) => CaptionKind::of(document, site),
            Self::Param(param) => {
                let crate::Export::Function(export) = document.exports.get(&param.function)? else {
                    return None;
                };
                let named = export
                    .signature
                    .params
                    .iter()
                    .any(|p| p.name == param.param);
                named.then_some(CaptionKind::Param)
            }
            Self::Field(field) => {
                let crate::Export::Function(entry) = document.exports.get(&field.function)? else {
                    return None;
                };
                field
                    .path
                    .resolve(&entry.signature)
                    .map(|_| CaptionKind::Field)
            }
        }
    }

    /// The site, for a site's caption.
    #[must_use]
    pub fn site(&self) -> Option<&Site> {
        match self {
            Self::Site(site) => Some(site),
            Self::Param(_) | Self::Field(_) => None,
        }
    }

    /// The declared function a parameter or field caption belongs to.
    #[must_use]
    pub fn function(&self) -> Option<&Name> {
        match self {
            Self::Site(_) => None,
            Self::Param(param) => Some(&param.function),
            Self::Field(field) => Some(&field.function),
        }
    }

    /// This target in the document `site`, the successor of its function's
    /// body, belongs to: the same parameter or field of that function.
    pub(super) fn of_function(&self, site: &Site) -> Option<Self> {
        let Unit::Function(function) = &site.unit else {
            return None;
        };
        if !site.path.is_empty() {
            return None;
        }
        Some(match self {
            Self::Site(_) => return None,
            Self::Param(param) => Self::Param(ParamTarget {
                function: function.clone(),
                param: param.param.clone(),
            }),
            Self::Field(field) => Self::Field(FieldTarget {
                function: function.clone(),
                path: field.path.clone(),
            }),
        })
    }
}

impl FieldPath {
    /// The type the path names in `signature`, when every step resolves.
    #[must_use]
    pub fn resolve<'s>(&self, signature: &'s Signature) -> Option<&'s Type> {
        if self.fields.is_empty() {
            return None;
        }
        let root = match &self.root {
            FieldRoot::Param(name) => {
                &signature
                    .params
                    .iter()
                    .find(|param| &param.name == name)?
                    .ty
            }
            FieldRoot::Result => &signature.result,
        };
        self.fields
            .iter()
            .try_fold(root, |ty, name| field_of(ty, name))
    }

    /// Every path `signature` resolves, ascending.
    #[must_use]
    pub fn every(signature: &Signature) -> Vec<Self> {
        let roots = signature
            .params
            .iter()
            .map(|param| (FieldRoot::Param(param.name.clone()), &param.ty))
            .chain(std::iter::once((FieldRoot::Result, &signature.result)));
        let mut out = Vec::new();
        for (root, ty) in roots {
            let mut pending = vec![(Vec::new(), ty)];
            while let Some((fields, ty)) = pending.pop() {
                for (name, ty) in record_fields(ty) {
                    let mut path = fields.clone();
                    path.push(name.to_owned());
                    out.push(Self {
                        root: root.clone(),
                        fields: path.clone(),
                    });
                    pending.push((path, ty));
                }
            }
        }
        out.sort();
        out
    }
}

/// The type of field `name` of a value of type `ty`, walking a list to its
/// element and an optional value to the value.
fn field_of<'t>(ty: &'t Type, name: &str) -> Option<&'t Type> {
    record_fields(ty)
        .into_iter()
        .find(|(field, _)| *field == name)
        .map(|(_, ty)| ty)
}

/// The named fields of the record a value of type `ty` is, walked to as
/// [`FieldPath`] says.
fn record_fields(ty: &Type) -> Vec<(&str, &Type)> {
    let mut ty = ty;
    loop {
        match ty {
            Type::Record(record) => {
                return record
                    .fields
                    .iter()
                    .map(|field| (field.name.as_str(), &field.ty))
                    .collect();
            }
            Type::List(item) => ty = item,
            Type::Union(members) => {
                let mut values = members
                    .iter()
                    .filter(|member| !matches!(member, Type::Null | Type::Absent));
                match (values.next(), values.next()) {
                    (Some(only), None) => ty = only,
                    _ => return Vec::new(),
                }
            }
            _ => return Vec::new(),
        }
    }
}

/// One caption: what it names and its text. Its JSON form names the target
/// by exactly one of `site`, `param` and `field`, beside `caption`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptionEntry {
    pub target: CaptionTarget,
    pub caption: Caption,
}

impl CaptionEntry {
    /// A site's caption.
    #[must_use]
    pub fn site(site: Site, caption: Caption) -> Self {
        Self {
            target: CaptionTarget::Site(site),
            caption,
        }
    }
}

/// A site's caption.
#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct SiteEntry<'a> {
    site: &'a Site,
    caption: &'a Caption,
}

/// A parameter's caption.
#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ParamEntry<'a> {
    param: &'a ParamTarget,
    caption: &'a Caption,
}

/// A field's caption.
#[derive(Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct FieldEntry<'a> {
    field: &'a FieldTarget,
    caption: &'a Caption,
}

/// One caption: its text and what it names, by exactly one of `site`,
/// `param` and `field`.
#[derive(Serialize, JsonSchema)]
#[serde(untagged)]
enum EntryWire<'a> {
    Site(SiteEntry<'a>),
    Param(ParamEntry<'a>),
    Field(FieldEntry<'a>),
}

impl<'a> From<&'a CaptionEntry> for EntryWire<'a> {
    fn from(entry: &'a CaptionEntry) -> Self {
        let caption = &entry.caption;
        match &entry.target {
            CaptionTarget::Site(site) => Self::Site(SiteEntry { site, caption }),
            CaptionTarget::Param(param) => Self::Param(ParamEntry { param, caption }),
            CaptionTarget::Field(field) => Self::Field(FieldEntry { field, caption }),
        }
    }
}

impl Serialize for CaptionEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        EntryWire::from(self).serialize(serializer)
    }
}

impl JsonSchema for CaptionEntry {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "CaptionEntry".into()
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        EntryWire::<'static>::json_schema(generator)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EntryFields {
    #[serde(default)]
    site: Option<Site>,
    #[serde(default)]
    param: Option<ParamTarget>,
    #[serde(default)]
    field: Option<FieldTarget>,
    caption: Caption,
}

impl<'de> Deserialize<'de> for CaptionEntry {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let fields = EntryFields::deserialize(deserializer)?;
        let target = match (fields.site, fields.param, fields.field) {
            (Some(site), None, None) => CaptionTarget::Site(site),
            (None, Some(param), None) => CaptionTarget::Param(param),
            (None, None, Some(field)) => CaptionTarget::Field(field),
            _ => {
                return Err(serde::de::Error::custom(
                    "a caption names exactly one of `site`, `param` and `field`",
                ));
            }
        };
        Ok(Self {
            target,
            caption: fields.caption,
        })
    }
}

impl crate::graph_tokens::TokenCodec for CaptionEntry {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(2);
        self.target.encode(out);
        self.caption.encode(out);
    }

    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            target: CaptionTarget::decode(input)?,
            caption: Caption::decode(input)?,
        })
    }
}

// CaptionTarget: Site = 0, Param = 1, Field = 2. FieldRoot: Param = 0, Result = 1.
impl crate::graph_tokens::TokenCodec for CaptionTarget {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        match self {
            Self::Site(site) => {
                out.sum(0, 1);
                site.encode(out);
            }
            Self::Param(param) => {
                out.sum(1, 1);
                param.encode(out);
            }
            Self::Field(field) => {
                out.sum(2, 1);
                field.encode(out);
            }
        }
    }

    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        use crate::graph_tokens::{TokenError, TokenReader};
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        Ok(match variant {
            0 => Self::Site(Site::decode(input)?),
            1 => Self::Param(ParamTarget::decode(input)?),
            2 => Self::Field(FieldTarget::decode(input)?),
            _ => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}

impl crate::graph_tokens::TokenCodec for ParamTarget {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(2);
        self.function.encode(out);
        self.param.encode(out);
    }
    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            function: Name::decode(input)?,
            param: Name::decode(input)?,
        })
    }
}

impl crate::graph_tokens::TokenCodec for FieldTarget {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(2);
        self.function.encode(out);
        self.path.encode(out);
    }
    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            function: Name::decode(input)?,
            path: FieldPath::decode(input)?,
        })
    }
}

impl crate::graph_tokens::TokenCodec for FieldPath {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        out.seq(2);
        self.root.encode(out);
        self.fields.encode(out);
    }
    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            root: FieldRoot::decode(input)?,
            fields: Vec::decode(input)?,
        })
    }
}

impl crate::graph_tokens::TokenCodec for FieldRoot {
    fn encode(&self, out: &mut crate::graph_tokens::TokenWriter) {
        match self {
            Self::Param(name) => {
                out.sum(0, 1);
                name.encode(out);
            }
            Self::Result => out.sum(1, 0),
        }
    }
    fn decode(
        input: &mut crate::graph_tokens::TokenReader<'_>,
    ) -> Result<Self, crate::graph_tokens::TokenError> {
        use crate::graph_tokens::{TokenError, TokenReader};
        let (variant, fields) = input.sum()?;
        Ok(match variant {
            0 => {
                TokenReader::expect_fields(fields, 1)?;
                Self::Param(Name::decode(input)?)
            }
            1 => {
                TokenReader::expect_fields(fields, 0)?;
                Self::Result
            }
            _ => return Err(TokenError::UnsupportedVariant { variant }),
        })
    }
}
