//! Source coordinates shared by readers, dialects and document rewrites.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Correspondence, Document, DocumentId, Site, SourceMapId};

/// UTF-8 byte offsets in authored or printed source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}
impl Span {
    /// An ordered half-open byte range. Source boundaries are checked by the map.
    pub fn new(start: usize, end: usize) -> Option<Self> {
        (start <= end).then_some(Self { start, end })
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SpanWire {
    start: usize,
    end: usize,
}
impl TryFrom<SpanWire> for Span {
    type Error = &'static str;
    fn try_from(value: SpanWire) -> Result<Self, Self::Error> {
        Self::new(value.start, value.end).ok_or("source range ends before it starts")
    }
}

/// The language of source, independent of a generated-source format version.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SourceLanguage {
    TypeScript,
    Python,
}

/// One document node's source locations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceSite {
    pub site: Site,
    pub ranges: Vec<Span>,
    /// A byte offset for a genuinely unspelled node.
    pub implicit: Option<usize>,
}

/// Source locations keyed by document identity and the SHA-256 of source bytes.
/// Sites are strictly ascending and unique. Validate decoded maps against their
/// document and source before using their locations.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceMap {
    pub source: [u8; 32],
    pub document: DocumentId,
    pub language: SourceLanguage,
    pub sites: Vec<SourceSite>,
}

/// The first failed check: document, digest, site, range, offset, then ordering.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SourceMapRefusal {
    #[error("source map names another document")]
    Document,
    #[error("source map digest differs from source")]
    Digest,
    #[error("source map site {site} does not exist")]
    Site { site: Site },
    #[error("source range {range:?} at {site} is outside source or splits UTF-8")]
    Range { site: Site, range: Span },
    #[error("implicit offset {offset} at {site} is outside source or splits UTF-8")]
    Implicit { site: Site, offset: usize },
    #[error("source map sites are unsorted or repeat at {site}")]
    Order { site: Site },
}

impl SourceMap {
    /// Build the one canonical ordering, then check every location.
    pub fn new(
        document: &Document,
        source: &str,
        language: SourceLanguage,
        sites: Vec<SourceSite>,
    ) -> Result<Self, SourceMapRefusal> {
        Self::of(document, document.identity(), source, language, sites)
    }

    /// [`Self::new`] for a document whose identity is already taken.
    pub fn new_identified(
        document: &crate::IdentifiedDocument,
        source: &str,
        language: SourceLanguage,
        sites: Vec<SourceSite>,
    ) -> Result<Self, SourceMapRefusal> {
        Self::of(document, document.id(), source, language, sites)
    }

    fn of(
        document: &Document,
        identity: DocumentId,
        source: &str,
        language: SourceLanguage,
        mut sites: Vec<SourceSite>,
    ) -> Result<Self, SourceMapRefusal> {
        sites.sort_by(|a, b| a.site.cmp(&b.site));
        let map = Self {
            source: Sha256::digest(source.as_bytes()).into(),
            document: identity,
            language,
            sites,
        };
        map.check(document, identity, source)?;
        Ok(map)
    }

    /// Check against the actual document and UTF-8 source, in a stable order.
    pub fn validate(&self, document: &Document, source: &str) -> Result<(), SourceMapRefusal> {
        self.check(document, document.identity(), source)
    }

    /// [`Self::validate`] against a document whose identity is already
    /// taken: nothing is encoded.
    pub fn validate_identified(
        &self,
        document: &crate::IdentifiedDocument,
        source: &str,
    ) -> Result<(), SourceMapRefusal> {
        self.check(document, document.id(), source)
    }

    fn check(
        &self,
        document: &Document,
        identity: DocumentId,
        source: &str,
    ) -> Result<(), SourceMapRefusal> {
        if self.document != identity {
            return Err(SourceMapRefusal::Document);
        }
        if self.source != <[u8; 32]>::from(Sha256::digest(source.as_bytes())) {
            return Err(SourceMapRefusal::Digest);
        }
        for entry in &self.sites {
            if document.node(&entry.site).is_none() {
                return Err(SourceMapRefusal::Site {
                    site: entry.site.clone(),
                });
            }
        }
        self.check_locations(source)
    }

    /// [`Self::validate`]'s checks that need only the source: its digest,
    /// every range and offset, and the site order. A reader whose document
    /// is no longer retained checks this much.
    pub fn validate_source(&self, source: &str) -> Result<(), SourceMapRefusal> {
        if self.source != <[u8; 32]>::from(Sha256::digest(source.as_bytes())) {
            return Err(SourceMapRefusal::Digest);
        }
        self.check_locations(source)
    }

    fn check_locations(&self, source: &str) -> Result<(), SourceMapRefusal> {
        for entry in &self.sites {
            for range in &entry.ranges {
                if range.start > range.end
                    || !source.is_char_boundary(range.start)
                    || !source.is_char_boundary(range.end)
                {
                    return Err(SourceMapRefusal::Range {
                        site: entry.site.clone(),
                        range: *range,
                    });
                }
            }
        }
        for entry in &self.sites {
            if let Some(offset) = entry.implicit
                && !source.is_char_boundary(offset)
            {
                return Err(SourceMapRefusal::Implicit {
                    site: entry.site.clone(),
                    offset,
                });
            }
        }
        self.check_order()
    }

    pub(crate) fn check_order(&self) -> Result<(), SourceMapRefusal> {
        for pair in self.sites.windows(2) {
            if pair[0].site >= pair[1].site {
                return Err(SourceMapRefusal::Order {
                    site: pair[1].site.clone(),
                });
            }
        }
        Ok(())
    }

    /// Move surviving sites, retaining even edited survivors' original ranges.
    pub fn rekey(&self, correspondence: &Correspondence) -> Option<Self> {
        if correspondence.base != self.document {
            return None;
        }
        let mut sites: Vec<_> = self
            .sites
            .iter()
            .filter_map(|entry| {
                Some(SourceSite {
                    site: correspondence.successor(&entry.site)?.clone(),
                    ranges: entry.ranges.clone(),
                    implicit: entry.implicit,
                })
            })
            .collect();
        sites.sort_by(|a, b| a.site.cmp(&b.site));
        Some(Self {
            source: self.source,
            document: correspondence.result,
            language: self.language,
            sites,
        })
    }

    /// The map's canonical form: its token body (`K-ID-003`'s grammar),
    /// which a store holds and the identity is the digest of.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        crate::graph_tokens::encode_body(self)
    }

    /// The map's identity: the logical digest of [`Self::canonical_bytes`].
    #[must_use]
    pub fn identity(&self) -> SourceMapId {
        SourceMapId::from_bytes(crate::graph_tokens::logical_digest(&self.canonical_bytes()))
    }

    /// The map `bytes` encode canonically, or why they do not.
    ///
    /// # Errors
    ///
    /// The decoder's refusal: bytes that are not the one canonical body of a
    /// well-formed map.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, String> {
        crate::graph_tokens::decode_body(bytes).map_err(|error| error.to_string())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceMapWire {
    source: [u8; 32],
    document: DocumentId,
    language: SourceLanguage,
    sites: Vec<SourceSite>,
}
impl TryFrom<SourceMapWire> for SourceMap {
    type Error = SourceMapRefusal;
    fn try_from(value: SourceMapWire) -> Result<Self, Self::Error> {
        let map = Self {
            source: value.source,
            document: value.document,
            language: value.language,
            sites: value.sites,
        };
        map.check_order()?;
        Ok(map)
    }
}

impl<'de> Deserialize<'de> for Span {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SpanWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl<'de> Deserialize<'de> for SourceMap {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        SourceMapWire::deserialize(deserializer)?
            .try_into()
            .map_err(serde::de::Error::custom)
    }
}

impl lash_vocabulary::sites::SourceLocations for SourceMap {
    fn source_start(&self, site: &Site) -> Option<usize> {
        self.sites
            .iter()
            .filter(|located| {
                located.site.unit == site.unit && site.path.starts_with(&located.site.path)
            })
            .filter_map(|located| {
                let start = located
                    .ranges
                    .first()
                    .map(|range| range.start)
                    .or(located.implicit)?;
                Some((located.site.path.len(), start))
            })
            .max_by_key(|(depth, _)| *depth)
            .map(|(_, start)| start)
    }
}
