//! Immutable namespaces, fills and scoped deferred grants.
//! A Link fills every document Import; selected execution reach is smaller.
//! Publication context is trusted host provenance, never guest authorization.
pub use crate::content_ids::{LinkId, PackageId};
use crate::{TurnControls, identity::SessionId, names::*, operations::*, providers::*, types::*};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Public by default. Internal fills require the exact package and publisher, not a shared root.
pub enum Visibility {
    #[default]
    Public,
    Internal,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Read answers are immediate and unsaved. Waited invocations are admitted durably.
pub enum OperationMode {
    Read,
    Waited,
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(transparent)]
/// One declared operation intent, retained as accepted metadata.
pub struct Intent(pub String);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// The serving signature/contract and accepted intents, controls and execution mode.
pub struct OperationOffer {
    pub requirement: OperationRequirement,
    pub intents: BTreeSet<Intent>,
    pub controls: TurnControls,
    pub mode: OperationMode,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// A published namespace: one path and its members. It need not own code.
pub struct Package {
    pub path: PackagePath,
    pub members: BTreeMap<Segment, Member>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// One package name's offered kind and visibility. Visibility does not expose a model tool.
pub struct Member {
    pub visibility: Visibility,
    pub kind: MemberKind,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// An operation offer or an Export of already linked code. Type/error aliases must preserve their Brand.
pub enum MemberKind {
    Operation(OperationOffer),
    Export { link: LinkId, export: ExportName },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Immutable linked code. Every document Import is filled, including Imports outside a selected start's reach.
pub struct Link {
    pub document: DocumentId,
    pub fills: BTreeMap<ImportSlot, Fill>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Chosen operation contract, linked Export, or scoped deferred grant. Deferred target outcomes live in the execution.
pub enum Fill {
    Operation(OperationRequirement),
    Export { link: LinkId, export: ExportName },
    Deferred { provider: Box<DeferredGrant> },
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
/// Logical owner/local provider identity, independent of the session/process hosting registry.
pub struct ProviderName {
    pub owner: String,
    pub key: String,
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
/// Host-supplied durable tenancy/storage-domain discriminator.
pub struct PublicationDomain(pub String);
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Authenticated host or session scope, supplied by Core rather than guest data.
pub enum PublicationScope {
    Host,
    Session(SessionId),
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
/// One configured publisher instance; two instances of one plugin remain distinct.
pub struct PublisherInstance(pub String);
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Trusted publication provenance from the registered publisher and authenticated domain/scope.
pub struct PublicationContext {
    pub domain: PublicationDomain,
    pub scope: PublicationScope,
    pub publisher: PublisherInstance,
    pub root: Segment,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// A pinned logical codec, readable format and immutable payload.
pub struct GrantPayload {
    pub codec: ProviderName,
    pub format: FormatVersion,
    pub value: serde_json::Value,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Immutable resolver config. A component is present only for recorded process-component config.
pub struct ProviderConfig {
    pub component: Option<ProcessComponentId>,
    pub format: FormatVersion,
    pub value: serde_json::Value,
    pub payload: Option<GrantPayload>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Authorized resolver scope/config/path and dynamic Json signature, pinned in the Link rather than a future match.
pub struct DeferredGrant {
    pub provider: ProviderName,
    pub context: PublicationContext,
    pub path: PackagePath,
    pub visibility: Visibility,
    pub signature: Signature,
    pub config: ProviderConfig,
    pub resolution_policy: DeferredResolutionPolicy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// A deferred path advertisement with no enumerated members. Only Operation Imports may use it.
pub struct DeferredPackage {
    pub path: PackagePath,
    pub visibility: Visibility,
    pub provider: ProviderName,
    pub config: ProviderConfig,
    pub resolution_policy: DeferredResolutionPolicy,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Enumerated or deferred namespace data. Enumerated roots are closed and never fall through.
pub enum PackageAdvertisement {
    Enumerated(Package),
    Deferred(Box<DeferredPackage>),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// Explicit host authorization for members/packages. Deferred paths must be separately declared.
pub struct Attachment {
    pub members: BTreeSet<QualifiedName>,
    pub packages: BTreeSet<PackagePath>,
    pub deferred: BTreeSet<PackagePath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
/// An advertisement paired with its accepted immutable publication provenance.
pub struct AcceptedAdvertisement {
    pub publication: PublicationRecord,
    pub advertisement: PackageAdvertisement,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Coherent accepted advertisements recorded for a round/start, with no fleet latest pointer.
pub struct NamespaceSnapshot {
    pub advertisements: Vec<AcceptedAdvertisement>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PublishedContent {
    Package(PackageId),
    Deferred(Box<DeferredPackage>),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PublicationRecord {
    pub context: PublicationContext,
    pub path: PackagePath,
    pub content: PublishedContent,
    pub code_links: BTreeSet<LinkId>,
    pub imported_publications: Vec<PublicationKey>,
}
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct PublicationKey {
    pub domain: PublicationDomain,
    pub scope: PublicationScope,
    pub publisher: PublisherInstance,
    pub path: PackagePath,
    pub content_digest: [u8; 32],
}

use crate::tokens::{TokenCodec, TokenError, TokenReader, TokenWriter};

// Field order is the canonical record order, independent of JSON object keys.
macro_rules! record_codec {
    ($ty:ty { $($field:ident),+ $(,)? }) => {
        impl TokenCodec for $ty {
            fn encode(&self, out: &mut TokenWriter) {
                out.seq([$(stringify!($field)),+].len() as u64);
                $(self.$field.encode(out);)+
            }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                input.expect_struct([$(stringify!($field)),+].len() as u64)?;
                Ok(Self { $($field: TokenCodec::decode(input)?),+ })
            }
        }
    };
}
macro_rules! text_codec {
    ($ty:ty, $parse:expr) => {
        impl TokenCodec for $ty {
            fn encode(&self, out: &mut TokenWriter) {
                out.text(&self.to_string());
            }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                ($parse)(input.text()?).map_err(|error| TokenError::Invalid(format!("{error}")))
            }
        }
    };
}
text_codec!(Segment, Segment::new);
text_codec!(PackagePath, |text: &str| PackagePath::parse(text));
record_codec!(ExportName { name, member });
record_codec!(QualifiedName {
    package,
    member,
    type_member
});
record_codec!(Brand { package, name });
record_codec!(Import { name, kind });
record_codec!(Link { document, fills });
record_codec!(Package { path, members });
record_codec!(Member { visibility, kind });
record_codec!(ProviderName { owner, key });
record_codec!(ProviderConfig {
    component,
    format,
    value,
    payload
});
record_codec!(GrantPayload {
    codec,
    format,
    value
});
record_codec!(PublicationContext {
    domain,
    scope,
    publisher,
    root
});
record_codec!(DeferredGrant {
    provider,
    context,
    path,
    visibility,
    signature,
    config,
    resolution_policy
});
record_codec!(DeferredPackage {
    path,
    visibility,
    provider,
    config,
    resolution_policy
});

impl TokenCodec for FormatVersion {
    fn encode(&self, out: &mut TokenWriter) {
        self.get().encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Self::new(u32::decode(input)?)
            .ok_or_else(|| TokenError::Invalid("zero format version".into()))
    }
}
impl TokenCodec for ProcessComponentId {
    fn encode(&self, out: &mut TokenWriter) {
        out.text(self.as_str());
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Self::new(input.text()?).map_err(|e| TokenError::Invalid(e.to_string()))
    }
}
macro_rules! string_newtype_codec {
    ($ty:ty) => {
        impl TokenCodec for $ty {
            fn encode(&self, out: &mut TokenWriter) {
                self.0.encode(out);
            }
            fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
                String::decode(input).map(Self)
            }
        }
    };
}
string_newtype_codec!(PublicationDomain);
string_newtype_codec!(PublisherInstance);
impl TokenCodec for PublicationScope {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Host => out.sum(0, 0),
            Self::Session(id) => {
                out.sum(1, 1);
                out.text(id.as_str());
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match sum(input, &[(0, 0), (1, 1)])? {
            0 => Self::Host,
            _ => Self::Session(
                SessionId::parse(input.text()?).map_err(|e| TokenError::Invalid(e.to_string()))?,
            ),
        })
    }
}
impl TokenCodec for DeferredResolutionPolicy {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(3);
        self.execution.as_secs().encode(out);
        self.execution.subsec_nanos().encode(out);
        match self.execution_policy {
            crate::ExecutionPolicy::Once => out.sum(0, 0),
            crate::ExecutionPolicy::Read => out.sum(1, 0),
            crate::ExecutionPolicy::Repeatable { retry } => {
                out.sum(2, 3);
                retry.max_attempts.get().encode(out);
                retry.backoff.base_delay_ms.encode(out);
                retry.backoff.max_delay_ms.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(3)?;
        let secs = u64::decode(input)?;
        let nanos = u32::decode(input)?;
        if nanos >= 1_000_000_000 {
            return Err(TokenError::Invalid("noncanonical duration".into()));
        }
        let execution_policy = match sum(input, &[(0, 0), (1, 0), (2, 3)])? {
            0 => crate::ExecutionPolicy::Once,
            1 => crate::ExecutionPolicy::Read,
            _ => crate::ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(u32::decode(input)?)
                    .ok_or_else(|| TokenError::Invalid("zero retry attempts".into()))?,
                u64::decode(input)?,
                u64::decode(input)?,
            ),
        };
        Ok(Self {
            execution: std::time::Duration::new(secs, nanos),
            execution_policy,
        })
    }
}
impl TokenCodec for TurnControls {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.finish_declaration()
            .map(|finish| finish.value_schema.as_value().clone())
            .encode(out);
        self.contains(crate::TurnControlKind::SwitchAgentFrame)
            .encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        let mut controls = Self::none();
        if let Some(value) = Option::<serde_json::Value>::decode(input)? {
            controls = controls.with_finish(
                crate::JsonSchema::admit(value).map_err(|e| TokenError::Invalid(e.to_string()))?,
            );
        }
        if bool::decode(input)? {
            controls = controls.with_switch_agent_frame();
        }
        Ok(controls)
    }
}
impl TokenCodec for Intent {
    fn encode(&self, out: &mut TokenWriter) {
        self.0.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        String::decode(input).map(Self)
    }
}
fn sum(input: &mut TokenReader<'_>, choices: &[(u32, u64)]) -> Result<u32, TokenError> {
    let (kind, fields) = input.sum()?;
    if choices.contains(&(kind, fields)) {
        Ok(kind)
    } else {
        Err(TokenError::Invalid(
            "unknown package variant or field count".into(),
        ))
    }
}
impl TokenCodec for Visibility {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(u32::from(*self == Self::Internal), 0);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(if sum(input, &[(0, 0), (1, 0)])? == 0 {
            Self::Public
        } else {
            Self::Internal
        })
    }
}
impl TokenCodec for OperationMode {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(u32::from(*self == Self::Waited), 0);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(if sum(input, &[(0, 0), (1, 0)])? == 0 {
            Self::Read
        } else {
            Self::Waited
        })
    }
}
record_codec!(OperationOffer {
    requirement,
    intents,
    controls,
    mode
});
impl TokenCodec for Convention {
    fn encode(&self, out: &mut TokenWriter) {
        out.sum(
            match self {
                Self::Direct => 0,
                Self::TsDirect => 1,
                Self::TsArgsList => 2,
                Self::PyDirect => 3,
            },
            0,
        );
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match sum(input, &[(0, 0), (1, 0), (2, 0), (3, 0)])? {
            0 => Self::Direct,
            1 => Self::TsDirect,
            2 => Self::TsArgsList,
            _ => Self::PyDirect,
        })
    }
}
impl TokenCodec for ImportKind {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Operation { signature } => {
                out.sum(0, 1);
                signature.encode(out);
            }
            Self::Code {
                signature,
                convention,
            } => {
                out.sum(1, 2);
                signature.encode(out);
                convention.encode(out);
            }
            Self::Type { brand } => {
                out.sum(2, 1);
                brand.encode(out);
            }
            Self::Error { kind } => {
                out.sum(3, 1);
                kind.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match sum(input, &[(0, 1), (1, 2), (2, 1), (3, 1)])? {
            0 => Self::Operation {
                signature: Signature::decode(input)?,
            },
            1 => Self::Code {
                signature: Signature::decode(input)?,
                convention: Convention::decode(input)?,
            },
            2 => Self::Type {
                brand: Brand::decode(input)?,
            },
            _ => Self::Error {
                kind: Brand::decode(input)?,
            },
        })
    }
}
impl TokenCodec for Fill {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Operation(requirement) => {
                out.sum(0, 1);
                requirement.encode(out);
            }
            Self::Export { link, export } => {
                out.sum(1, 2);
                link.encode(out);
                export.encode(out);
            }
            Self::Deferred { provider } => {
                out.sum(2, 1);
                provider.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match sum(input, &[(0, 1), (1, 2), (2, 1)])? {
            0 => Self::Operation(OperationRequirement::decode(input)?),
            1 => Self::Export {
                link: LinkId::decode(input)?,
                export: ExportName::decode(input)?,
            },
            _ => Self::Deferred {
                provider: Box::decode(input)?,
            },
        })
    }
}
impl TokenCodec for MemberKind {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Operation(offer) => {
                out.sum(0, 1);
                offer.encode(out);
            }
            Self::Export { link, export } => {
                out.sum(1, 2);
                link.encode(out);
                export.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        Ok(match sum(input, &[(0, 1), (1, 2)])? {
            0 => Self::Operation(OperationOffer::decode(input)?),
            _ => Self::Export {
                link: LinkId::decode(input)?,
                export: ExportName::decode(input)?,
            },
        })
    }
}
impl Link {
    /// Content identity of the immutable document and every chosen fill.
    pub fn identity(&self) -> LinkId {
        LinkId::from_bytes(blake3::derive_key(
            "lash.link.v1",
            &crate::tokens::encode_body(self),
        ))
    }
}
impl Package {
    /// Content identity of this namespace; routes and publication provenance are separate.
    pub fn identity(&self) -> PackageId {
        PackageId::from_bytes(blake3::derive_key(
            "lash.package.v1",
            &crate::tokens::encode_body(self),
        ))
    }
}

impl PublicationRecord {
    /// Immutable scoped provenance key. The digest covers canonical structural
    /// tokens for content and retention edges, independent of display labels.
    #[must_use]
    pub fn key(&self) -> PublicationKey {
        let mut out = TokenWriter::new();
        out.seq(4);
        out.text("lash.publication.v1");
        self.content.encode(&mut out);
        self.code_links.encode(&mut out);
        self.imported_publications.encode(&mut out);
        PublicationKey {
            domain: self.context.domain.clone(),
            scope: self.context.scope.clone(),
            publisher: self.context.publisher.clone(),
            path: self.path.clone(),
            content_digest: *blake3::hash(&out.into_bytes()).as_bytes(),
        }
    }
}

impl TokenCodec for PublishedContent {
    fn encode(&self, out: &mut TokenWriter) {
        match self {
            Self::Package(id) => {
                out.sum(0, 1);
                id.encode(out);
            }
            Self::Deferred(package) => {
                out.sum(1, 1);
                package.encode(out);
            }
        }
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        let (variant, fields) = input.sum()?;
        TokenReader::expect_fields(fields, 1)?;
        match variant {
            0 => Ok(Self::Package(PackageId::decode(input)?)),
            1 => Ok(Self::Deferred(Box::new(DeferredPackage::decode(input)?))),
            variant => Err(TokenError::UnsupportedVariant { variant }),
        }
    }
}
impl TokenCodec for PublicationKey {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(5);
        self.domain.encode(out);
        self.scope.encode(out);
        self.publisher.encode(out);
        self.path.encode(out);
        out.bytes(&self.content_digest);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(5)?;
        Ok(Self {
            domain: PublicationDomain::decode(input)?,
            scope: PublicationScope::decode(input)?,
            publisher: PublisherInstance::decode(input)?,
            path: PackagePath::decode(input)?,
            content_digest: input.bytes()?.try_into().map_err(|_| {
                TokenError::Invalid("publication digest must contain 32 bytes".into())
            })?,
        })
    }
}

/// Authenticated publication and authored-cell roots retained by a callable start.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallableOrigin {
    pub publications: BTreeSet<PublicationKey>,
    pub authored: BTreeSet<LinkId>,
}
