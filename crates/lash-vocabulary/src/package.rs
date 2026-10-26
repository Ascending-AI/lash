//! Accepted publication provenance retained by the host.
use lash_kernel_value::{
    names::*,
    package::*,
    tokens::{TokenCodec, TokenError, TokenReader, TokenWriter},
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

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
