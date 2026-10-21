//! A build's validated functions as a zero-copy image (FIG-6843).
//!
//! The build writes each released function's identity, name,
//! implementation facts and body census beside its definition's token body.
//! A process registers an image without decoding a definition, and decodes
//! one only when it first reads it. The image is read only by the build
//! that wrote it, so its layout is no stored format.
//!
//! Layout, every integer a little-endian `u32`:
//!
//! - a header: the function count, the basis count, the dangling count
//!   and the names' length;
//! - the basis: each identity the functions were validated against,
//!   ascending;
//! - the dangling callees: each function a body names that neither the
//!   image nor its basis holds, ascending;
//! - one fixed record per function, ascending by identity: the identity,
//!   the definition's range, the name's range, the flags, the census
//!   counts, the occurrence paths' range, the kernel version and the range
//!   of the functions the body lists;
//! - each record's index, in name order;
//! - the names, one UTF-8 text the name ranges index;
//! - the blob the other ranges index.

use std::collections::BTreeSet;

use crate::graph_tokens::TokenError;
use crate::{FunctionDefinition, FunctionId, Site, ValidatedFunctions};

#[cfg(test)]
std::thread_local! {
    /// How many definitions this thread decoded from images.
    pub(crate) static DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

const HEADER: usize = 16;
const ID: usize = 32;
const RECORD: usize = ID + 14 * 4;

const HAS_BODY: u32 = 1;
const STATES_NATIVE: u32 = 1 << 1;
const CENSUS: u32 = 1 << 2;
const CLOSURES: u32 = 1 << 3;

/// What a machine's preparation reserves for a library body before it
/// compiles it: its codes (the body and each closure), blocks, statements
/// and property read sites, whether it declares a closure, and the child
/// site of each action whose occurrence it counts. The machine computes it
/// (`lash_kernel_vm::body_census`), and a build writes it into its image
/// so that a process prepares a release without walking its bodies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BodyCensus {
    pub codes: u32,
    pub blocks: u32,
    pub stmts: u32,
    pub sites: u32,
    pub closures: bool,
    pub occurrences: Vec<Site>,
}

/// A release image the build wrote is malformed: a defect of the build.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the release image is malformed: {0}")]
pub struct ImageError(&'static str);

/// Validated functions as their build's image holds them, read in place.
#[derive(Clone, Copy, Debug)]
pub struct ReleasedFunctions {
    bytes: &'static [u8],
    count: usize,
    basis: usize,
    dangling: usize,
    /// Every name, checked as UTF-8 once.
    names: &'static str,
}

impl ReleasedFunctions {
    /// Reads the image [`ValidatedFunctions::to_image`] wrote, checking that
    /// every range it states lies inside it.
    ///
    /// # Errors
    ///
    /// [`ImageError`] when it does not.
    pub fn read(bytes: &'static [u8]) -> Result<Self, ImageError> {
        let image = Self::over_frame(bytes)?;
        let blob = bytes.len() - image.blob();
        for at in 0..image.count {
            let fits =
                |range: (usize, usize)| range.0.checked_add(range.1).is_some_and(|end| end <= blob);
            let (name, len) = image.range(at, 2);
            if !fits(image.range(at, 0))
                || image.names.get(name..name + len).is_none()
                || !fits(image.range(at, 9))
                || !fits(image.range(at, 12))
            {
                return Err(ImageError("a record states a range outside it"));
            }
            if image.name_order(at) >= image.count {
                return Err(ImageError("its name order names no record"));
            }
        }
        Ok(image)
    }

    /// Opens an image already validated by the parent of this worker. Only
    /// its envelope is read; record validation and merging belong to install.
    pub fn over_frame(bytes: &'static [u8]) -> Result<Self, ImageError> {
        let word = |at: usize| -> Result<usize, ImageError> {
            bytes
                .get(at..at + 4)
                .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]) as usize)
                .ok_or(ImageError("it ends inside its header"))
        };
        let (count, basis, dangling, names) = (word(0)?, word(4)?, word(8)?, word(12)?);
        let mut image = Self {
            bytes,
            count,
            basis,
            dangling,
            names: "",
        };
        let start = image.names_start();
        let names = bytes
            .get(start..start + names)
            .ok_or(ImageError("it ends inside its tables"))?;
        image.names =
            std::str::from_utf8(names).map_err(|_| ImageError("its names are not UTF-8"))?;
        if image.blob() > bytes.len() {
            return Err(ImageError("it ends inside its tables"));
        }
        Ok(image)
    }

    /// How many functions it holds.
    pub fn len(&self) -> usize {
        self.count
    }

    pub fn is_empty(&self) -> bool {
        self.count == 0
    }

    /// Function `at`'s identity, ascending in `at`.
    pub fn id(&self, at: usize) -> FunctionId {
        id_at(self.bytes, self.records() + at * RECORD)
    }

    /// Every function's identity, ascending.
    pub fn ids(&self) -> impl ExactSizeIterator<Item = FunctionId> + '_ {
        (0..self.count).map(|at| self.id(at))
    }

    /// Every function the functions were validated against, ascending.
    pub fn basis(&self) -> impl ExactSizeIterator<Item = FunctionId> + '_ {
        (0..self.basis).map(|at| id_at(self.bytes, HEADER + at * ID))
    }

    /// Every function a body names that neither the image nor its basis
    /// holds, ascending.
    pub fn dangling(&self) -> impl ExactSizeIterator<Item = FunctionId> + '_ {
        (0..self.dangling).map(|at| id_at(self.bytes, HEADER + (self.basis + at) * ID))
    }

    /// Function `at`'s name, as its definition states it.
    pub fn name(&self, at: usize) -> &'static str {
        let (start, len) = self.range(at, 2);
        self.names.get(start..start + len).unwrap_or_default()
    }

    /// The index of the function `rank`th in name order.
    pub fn name_order(&self, rank: usize) -> usize {
        self.word(self.records() + self.count * RECORD + rank * 4) as usize
    }

    /// Whether function `at` has a kernel body.
    pub fn has_body(&self, at: usize) -> bool {
        self.flags(at) & HAS_BODY != 0
    }

    /// Whether function `at`'s definition states a native implementation.
    pub fn states_native(&self, at: usize) -> bool {
        self.flags(at) & STATES_NATIVE != 0
    }

    /// Function `at`'s body census, when the build wrote one.
    pub fn census(&self, at: usize) -> Option<BodyCensus> {
        let flags = self.flags(at);
        if flags & CENSUS == 0 {
            return None;
        }
        let field = |index: usize| self.word(self.records() + at * RECORD + ID + index * 4);
        let occurrences = crate::graph_tokens::decode_body(self.slice(self.range(at, 9))).ok()?;
        Some(BodyCensus {
            codes: field(5),
            blocks: field(6),
            stmts: field(7),
            sites: field(8),
            closures: flags & CLOSURES != 0,
            occurrences,
        })
    }

    /// The kernel version function `at`'s definition is written for.
    pub fn kernel(&self, at: usize) -> u32 {
        self.word(self.records() + at * RECORD + ID + 11 * 4)
    }

    /// Every function function `at`'s body lists, by identity, with the
    /// name its definition carries, ascending by identity
    /// ([`crate::FunctionBody::functions`]).
    pub fn listed(&self, at: usize) -> impl Iterator<Item = (FunctionId, &'static str)> {
        let mut rest = self.slice(self.range(at, 12));
        std::iter::from_fn(move || {
            let id = rest.get(..ID)?;
            let len = rest.get(ID..ID + 4)?;
            let len = u32::from_le_bytes([len[0], len[1], len[2], len[3]]) as usize;
            let name = rest.get(ID + 4..ID + 4 + len)?;
            let function = id_at(id, 0);
            rest = &rest[ID + 4 + len..];
            Some((function, std::str::from_utf8(name).unwrap_or_default()))
        })
    }

    /// Function `at`'s definition's canonical token body
    /// ([`crate::graph_tokens::encode_body`]).
    pub fn definition_bytes(&self, at: usize) -> &'static [u8] {
        self.slice(self.range(at, 0))
    }

    /// Decodes function `at`'s definition.
    ///
    /// # Errors
    ///
    /// Any token the encoding does not hold.
    pub fn definition(&self, at: usize) -> Result<FunctionDefinition, TokenError> {
        #[cfg(test)]
        DECODED.with(|decoded| decoded.set(decoded.get() + 1));
        crate::graph_tokens::decode_body(self.definition_bytes(at))
    }

    fn records(&self) -> usize {
        HEADER + (self.basis + self.dangling) * ID
    }

    fn names_start(&self) -> usize {
        self.records() + self.count * (RECORD + 4)
    }

    fn blob(&self) -> usize {
        self.names_start() + self.names.len()
    }

    fn word(&self, at: usize) -> u32 {
        let word = &self.bytes[at..at + 4];
        u32::from_le_bytes([word[0], word[1], word[2], word[3]])
    }

    fn flags(&self, at: usize) -> u32 {
        self.word(self.records() + at * RECORD + ID + 4 * 4)
    }

    /// The `(offset, length)` pair at field `field` of record `at`.
    fn range(&self, at: usize, field: usize) -> (usize, usize) {
        let base = self.records() + at * RECORD + ID + field * 4;
        (self.word(base) as usize, self.word(base + 4) as usize)
    }

    fn slice(&self, (offset, len): (usize, usize)) -> &'static [u8] {
        let start = self.blob() + offset;
        &self.bytes[start..start + len]
    }
}

fn id_at(bytes: &[u8], at: usize) -> FunctionId {
    let mut digest = [0; ID];
    digest.copy_from_slice(&bytes[at..at + ID]);
    FunctionId::from_bytes(digest)
}

impl ValidatedFunctions {
    /// The image a build keeps them as ([`ReleasedFunctions`]), each body's
    /// census as `census` gives it. It is read back only by the same build.
    #[must_use]
    pub fn to_image(&self, census: &dyn Fn(&FunctionDefinition) -> Option<BodyCensus>) -> Vec<u8> {
        self.to_image_with(&|_, definition| census(definition))
    }

    pub(crate) fn to_image_with(
        &self,
        census: &dyn Fn(&FunctionId, &FunctionDefinition) -> Option<BodyCensus>,
    ) -> Vec<u8> {
        let mut functions: Vec<_> = self.iter().collect();
        functions.sort_by_key(|(function, _)| **function);
        let basis: BTreeSet<FunctionId> = self.basis().iter().copied().collect();
        let held: BTreeSet<FunctionId> = functions.iter().map(|(function, _)| **function).collect();
        let dangling: BTreeSet<FunctionId> = functions
            .iter()
            .filter_map(|(_, definition)| definition.body())
            .flat_map(|body| body.functions.keys().copied())
            .filter(|callee| !basis.contains(callee) && !held.contains(callee))
            .collect();
        let mut blob = Vec::new();
        let mut put = |bytes: &[u8]| {
            let offset = blob.len() as u32;
            blob.extend_from_slice(bytes);
            [offset, bytes.len() as u32]
        };
        let mut names = String::new();
        let mut records = Vec::with_capacity(functions.len() * RECORD);
        for (function, definition) in &functions {
            let body = put(&crate::graph_tokens::encode_body(*definition));
            let name = [names.len() as u32, definition.name.as_str().len() as u32];
            names.push_str(definition.name.as_str());
            let census = census(function, definition);
            let mut flags = 0;
            if definition.body().is_some() {
                flags |= HAS_BODY;
            }
            if definition.has_native() {
                flags |= STATES_NATIVE;
            }
            let mut paths = Vec::new();
            let counts = match &census {
                Some(census) => {
                    flags |= CENSUS;
                    if census.closures {
                        flags |= CLOSURES;
                    }
                    paths = crate::graph_tokens::encode_body(&census.occurrences);
                    [census.codes, census.blocks, census.stmts, census.sites]
                }
                None => [0; 4],
            };
            let occurrences = put(&paths);
            let mut listed = Vec::new();
            for (callee, name) in definition.body().iter().flat_map(|body| &body.functions) {
                listed.extend_from_slice(callee.as_bytes());
                listed.extend((name.as_str().len() as u32).to_le_bytes());
                listed.extend_from_slice(name.as_str().as_bytes());
            }
            let listed = put(&listed);
            records.extend_from_slice(function.as_bytes());
            for word in body
                .into_iter()
                .chain(name)
                .chain([flags])
                .chain(counts)
                .chain(occurrences)
                .chain([definition.kernel])
                .chain(listed)
            {
                records.extend(word.to_le_bytes());
            }
        }
        let mut by_name: Vec<usize> = (0..functions.len()).collect();
        by_name.sort_by(|a, b| {
            let name = |at: usize| functions[at].1.name.as_str().as_bytes();
            name(*a).cmp(name(*b)).then(a.cmp(b))
        });
        let mut out = Vec::new();
        for count in [functions.len(), basis.len(), dangling.len(), names.len()] {
            out.extend((count as u32).to_le_bytes());
        }
        for function in basis.iter().chain(&dangling) {
            out.extend_from_slice(function.as_bytes());
        }
        out.extend(records);
        for at in by_name {
            out.extend((at as u32).to_le_bytes());
        }
        out.extend_from_slice(names.as_bytes());
        out.extend(blob);
        out
    }
}
