//! Source coordinates shared by readers, dialects and document rewrites.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Correspondence, Document, DocumentId, EncodeError, Site, SourceMapId};

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
    #[error("cannot identify source map document: {0}")]
    Identity(#[from] EncodeError),
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
        mut sites: Vec<SourceSite>,
    ) -> Result<Self, SourceMapRefusal> {
        sites.sort_by(|a, b| a.site.cmp(&b.site));
        let map = Self {
            source: Sha256::digest(source.as_bytes()).into(),
            document: document.identity()?,
            language,
            sites,
        };
        map.validate(document, source)?;
        Ok(map)
    }

    /// Check against the actual document and UTF-8 source, in a stable order.
    pub fn validate(&self, document: &Document, source: &str) -> Result<(), SourceMapRefusal> {
        if self.document != document.identity()? {
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

    /// Canonical JSON, using the document vocabulary's identity encoder.
    pub fn canonical_bytes(&self) -> Result<Vec<u8>, EncodeError> {
        crate::canonical::canonical_bytes(self)
    }

    /// The map's identity: the digest of [`Self::canonical_bytes`].
    pub fn identity(&self) -> Result<SourceMapId, EncodeError> {
        Ok(SourceMapId::from_bytes(crate::canonical::digest_bytes(
            "lash-kernel-source-map",
            &self.canonical_bytes()?,
        )))
    }

    /// The map `bytes` encode canonically, or why they do not.
    ///
    /// # Errors
    ///
    /// The decoder's message, or that the bytes are not the canonical
    /// encoding of the map they decode to.
    pub fn from_canonical_bytes(bytes: &[u8]) -> Result<Self, String> {
        let map: Self = serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
        if map.canonical_bytes().map_err(|error| error.message)? != bytes {
            return Err("the source map is not in its canonical encoding".to_owned());
        }
        Ok(map)
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
