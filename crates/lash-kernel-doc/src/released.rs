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

use std::collections::{BTreeMap, BTreeSet};

use crate::{FunctionDefinition, ValidatedFunctions};
use lash_kernel_value::names::FunctionId;
use lash_kernel_value::sites::Site;
use lash_kernel_value::tokens::TokenError;

#[cfg(test)]
std::thread_local! {
    /// How many definitions this thread decoded from images.
    pub(crate) static DECODED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(any(test, feature = "perf-witness"))]
std::thread_local! {
    static ADMISSION_EDGES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

const HEADER: usize = 16;
const ID: usize = 32;
const RECORD: usize = ID + 16 * 4;

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
                || !fits(image.range(at, 14))
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
        let occurrences =
            lash_kernel_value::tokens::decode_body(self.slice(self.range(at, 9))).ok()?;
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

    /// Validated operation requirements, read without opening the function body.
    pub fn imports(
        &self,
        at: usize,
    ) -> Result<
        std::collections::BTreeMap<lash_kernel_value::ImportSlot, lash_kernel_value::Import>,
        TokenError,
    > {
        lash_kernel_value::tokens::decode_body(self.slice(self.range(at, 14)))
    }

    /// Function `at`'s definition's canonical token body
    /// ([`lash_kernel_value::tokens::encode_body`]).
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
        crate::document_work::note_helper(|work| work.body_decodes += 1);
        lash_kernel_value::tokens::decode_body(self.definition_bytes(at))
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
            let body = put(&lash_kernel_value::tokens::encode_body(*definition));
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
                    paths = lash_kernel_value::tokens::encode_body(&census.occurrences);
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
            let imports = put(&lash_kernel_value::tokens::encode_body(
                &definition
                    .body()
                    .map(|body| body.imports.clone())
                    .unwrap_or_default(),
            ));
            records.extend_from_slice(function.as_bytes());
            for word in body
                .into_iter()
                .chain(name)
                .chain([flags])
                .chain(counts)
                .chain(occurrences)
                .chain([definition.kernel])
                .chain(listed)
                .chain(imports)
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

/// A saved continuation needs a build helper absent from the installed image.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    schemars::JsonSchema,
    thiserror::Error,
)]
#[error("build helper {function} is unavailable: {reason}")]
#[serde(deny_unknown_fields)]
pub struct HelperUnavailable {
    pub reason: lash_kernel_value::HelperUnavailableReason,
    pub function: FunctionId,
}

/// Immutable build-owned code, indexed by exact identity. Construction proves
/// static closure once; compatibility reads only identity and membership.
#[derive(Clone, Debug)]
pub struct BuildHelpers {
    identity: lash_kernel_value::HelperLibraryId,
    library_identities: BTreeSet<lash_kernel_value::HelperLibraryId>,
    current: lash_kernel_value::HelperLibraryId,
    claim_identity: lash_kernel_value::HelperLibraryId,
    generations: std::sync::Arc<BTreeMap<FunctionId, lash_kernel_value::claims::HelperGeneration>>,
    archives: BTreeMap<lash_kernel_value::HelperLibraryId, u32>,
    functions: crate::FunctionRegistry,
    members: std::collections::HashMap<FunctionId, HelperMember>,
    member_order: BTreeSet<FunctionId>,
}

#[derive(Clone, Debug)]
struct HelperMember {
    generation: Option<lash_kernel_value::claims::HelperGeneration>,
    callees: BTreeSet<FunctionId>,
}

impl BuildHelpers {
    /// Validate an externally assembled image once at installation. The member
    /// universe is the roots closed over the build-owned callee metadata.
    pub fn new(
        functions: crate::FunctionRegistry,
        roots: impl IntoIterator<Item = FunctionId>,
    ) -> Result<Self, HelperUnavailable> {
        let mut pending: Vec<_> = roots.into_iter().collect();
        let mut callees = std::collections::BTreeMap::new();
        while let Some(function) = pending.pop() {
            if callees.contains_key(&function) {
                continue;
            }
            let registered = functions.get(&function).ok_or(HelperUnavailable {
                reason: lash_kernel_value::HelperUnavailableReason::Unknown,
                function,
            })?;
            let children: BTreeSet<_> = registered.listed().into_iter().map(|(id, _)| id).collect();
            pending.extend(children.iter().copied());
            callees.insert(function, children);
        }
        crate::document_work::note_helper(|work| work.identity_hashes += 1);
        let identity = lash_kernel_value::HelperLibraryId::of_members(callees.keys().copied());
        let member_order = callees.keys().copied().collect();
        let members = callees
            .into_iter()
            .map(|(id, callees)| {
                (
                    id,
                    HelperMember {
                        generation: None,
                        callees,
                    },
                )
            })
            .collect();
        Ok(Self {
            identity,
            library_identities: BTreeSet::from([identity]),
            current: identity,
            claim_identity: identity,
            generations: Default::default(),
            archives: Default::default(),
            functions,
            members,
            member_order,
        })
    }

    pub fn empty() -> std::sync::Arc<Self> {
        static EMPTY: std::sync::OnceLock<std::sync::Arc<BuildHelpers>> =
            std::sync::OnceLock::new();
        std::sync::Arc::clone(EMPTY.get_or_init(|| {
            std::sync::Arc::new(Self {
                identity: lash_kernel_value::HelperLibraryId::of_members([]),
                library_identities: BTreeSet::from([
                    lash_kernel_value::HelperLibraryId::of_members([]),
                ]),
                current: lash_kernel_value::HelperLibraryId::of_members([]),
                claim_identity: lash_kernel_value::HelperLibraryId::of_members([]),
                generations: Default::default(),
                archives: Default::default(),
                functions: crate::FunctionRegistry::declarations(),
                members: Default::default(),
                member_order: Default::default(),
            })
        }))
    }

    /// The install-validated member universe, in canonical order.
    pub fn members(&self) -> impl ExactSizeIterator<Item = &FunctionId> {
        self.member_order.iter()
    }

    /// Opens metadata whose identity and closure were checked by the installing
    /// parent. Like ReleasedFunctions::over_frame, this does no content hashing.
    pub fn from_frame(
        identity: lash_kernel_value::HelperLibraryId,
        functions: crate::FunctionRegistry,
        members: BTreeSet<FunctionId>,
    ) -> Result<Self, HelperUnavailable> {
        let mut callees = std::collections::BTreeMap::new();
        for function in &members {
            let registered = functions.get(function).ok_or(HelperUnavailable {
                reason: lash_kernel_value::HelperUnavailableReason::Unknown,
                function: *function,
            })?;
            let children: BTreeSet<_> = registered.listed().into_iter().map(|(id, _)| id).collect();
            for child in &children {
                if !members.contains(child) {
                    return Err(HelperUnavailable {
                        reason: lash_kernel_value::HelperUnavailableReason::Unknown,
                        function: *child,
                    });
                }
            }
            callees.insert(*function, children);
        }
        Ok(Self {
            identity,
            library_identities: BTreeSet::from([identity]),
            current: identity,
            claim_identity: identity,
            generations: Default::default(),
            archives: Default::default(),
            functions,
            members: callees
                .into_iter()
                .map(|(id, callees)| {
                    (
                        id,
                        HelperMember {
                            generation: None,
                            callees,
                        },
                    )
                })
                .collect(),
            member_order: members,
        })
    }

    /// Historical manifests authenticated by the installing parent and covered
    /// by this exact member universe. These are availability claims only.
    pub fn with_library_identities(
        mut self,
        identities: impl IntoIterator<Item = lash_kernel_value::HelperLibraryId>,
    ) -> Self {
        self.library_identities.extend(identities);
        self
    }

    /// Bind the authenticated first-generation index once, before serving runs.
    pub fn with_claim_generations(
        mut self,
        current: lash_kernel_value::HelperLibraryId,
        generations: BTreeMap<FunctionId, lash_kernel_value::claims::HelperGeneration>,
    ) -> Result<Self, HelperUnavailable> {
        use lash_kernel_value::tokens::TokenCodec as _;
        self.archives.clear();
        self.members
            .values_mut()
            .for_each(|member| member.generation = None);
        for (function, claim) in &generations {
            let lash_kernel_value::claims::HelperGeneration {
                archive,
                generation,
                package,
            } = claim;
            if !self.contains(function) || *generation == 0 || !self.supports_library(package) {
                return Err(HelperUnavailable {
                    function: *function,
                    reason: lash_kernel_value::HelperUnavailableReason::Corrupt,
                });
            }
            self.members
                .get_mut(function)
                .unwrap_or_else(|| unreachable!("checked member"))
                .generation = Some(*claim);
            self.archives
                .entry(*archive)
                .and_modify(|held| *held = (*held).max(*generation))
                .or_insert(*generation);
        }
        let mut tokens = lash_kernel_value::tokens::TokenWriter::new();
        tokens.bytes(b"helper-claim-index");
        self.identity.encode(&mut tokens);
        current.encode(&mut tokens);
        generations.encode(&mut tokens);
        self.claim_identity = lash_kernel_value::HelperLibraryId::from_bytes(
            lash_kernel_value::tokens::logical_digest(&tokens.into_bytes()),
        );
        self.current = current;
        self.generations = std::sync::Arc::new(generations);
        Ok(self)
    }

    pub fn current_identity(&self) -> lash_kernel_value::HelperLibraryId {
        self.current
    }
    pub fn claim_identity(&self) -> lash_kernel_value::HelperLibraryId {
        self.claim_identity
    }
    pub fn claim_generations(
        &self,
    ) -> &BTreeMap<FunctionId, lash_kernel_value::claims::HelperGeneration> {
        &self.generations
    }
    pub fn claim_capabilities(&self) -> lash_kernel_value::claims::HelperRequirements {
        lash_kernel_value::claims::HelperRequirements {
            archives: self.archives.clone(),
            outside: Default::default(),
        }
    }

    /// Classify and project one helper through the installed membership index.
    /// Called by the identity-keyed inspection cache, never by claim admission.
    pub fn require(
        &self,
        function: FunctionId,
        required: &mut lash_kernel_value::claims::NodeRequirements,
    ) -> bool {
        let Some(claim) = self.members.get(&function) else {
            return false;
        };
        if let Some(claim) = &claim.generation {
            let lash_kernel_value::claims::HelperGeneration {
                archive,
                generation,
                package,
            } = claim;
            required
                .helpers
                .archives
                .entry(*archive)
                .and_modify(|held| *held = (*held).max(*generation))
                .or_insert(*generation);
            required.libraries.insert(*package);
        } else {
            required
                .helpers
                .outside
                .entry(self.current)
                .or_default()
                .insert(function);
        }
        true
    }

    pub fn library_identities(&self) -> &BTreeSet<lash_kernel_value::HelperLibraryId> {
        &self.library_identities
    }

    pub fn supports_library(&self, identity: &lash_kernel_value::HelperLibraryId) -> bool {
        self.library_identities.contains(identity)
    }

    pub fn identity(&self) -> lash_kernel_value::HelperLibraryId {
        self.identity
    }
    pub fn contains(&self, function: &FunctionId) -> bool {
        self.members.contains_key(function)
    }
    pub fn callees(&self, function: &FunctionId) -> Option<&BTreeSet<FunctionId>> {
        let callees = self.members.get(function).map(|member| &member.callees);
        #[cfg(any(test, feature = "perf-witness"))]
        ADMISSION_EDGES
            .with(|count| count.set(count.get() + callees.map_or(0, |edges| edges.len() as u64)));
        callees
    }
    pub fn registered(&self, function: &FunctionId) -> Option<&crate::RegisteredFunction> {
        self.contains(function)
            .then(|| self.functions.get(function))
            .flatten()
    }
    pub fn functions(&self) -> &crate::FunctionRegistry {
        &self.functions
    }
}

impl BuildHelpers {
    /// Static origin closure. This is admission/parking work, never compatibility work.
    pub fn reach(
        &self,
        roots: &crate::LibraryDependencies,
    ) -> Result<BTreeSet<FunctionId>, HelperUnavailable> {
        let mut pending: Vec<_> = roots.iter().copied().collect();
        let mut reached = BTreeSet::new();
        while let Some(function) = pending.pop() {
            if reached.insert(function) {
                pending.extend(
                    self.callees(&function)
                        .ok_or(HelperUnavailable {
                            reason: lash_kernel_value::HelperUnavailableReason::Unknown,
                            function,
                        })?
                        .iter()
                        .copied(),
                );
            }
        }
        Ok(reached)
    }
    /// Semantic helper edges examined on this thread since the last read.
    #[cfg(any(test, feature = "perf-witness"))]
    pub fn take_admission_edge_visits() -> u64 {
        ADMISSION_EDGES.replace(0)
    }

    /// Exact admission closure, skipping absent roots as `admits` does.
    /// `reach` separately refuses a missing helper when closure is required.
    pub fn admitted(&self, roots: &crate::LibraryDependencies) -> BTreeSet<FunctionId> {
        let mut pending: Vec<_> = roots.iter().copied().collect();
        let mut seen = BTreeSet::new();
        let mut reached = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            if let Some(callees) = self.callees(&id) {
                reached.insert(id);
                pending.extend(callees.iter().copied());
            }
        }
        reached
    }

    pub fn admits(&self, roots: &crate::LibraryDependencies, function: &FunctionId) -> bool {
        self.admitted(roots).contains(function)
    }
}
impl crate::FunctionCatalog for BuildHelpers {
    fn named_helper(&self, name: &str) -> Option<FunctionId> {
        self.functions
            .named_helper(name)
            .filter(|id| self.contains(id))
    }

    fn helper_library(&self) -> Option<&Self> {
        Some(self)
    }

    fn definition(&self, function: &FunctionId) -> Option<&FunctionDefinition> {
        self.registered(function)
            .map(|registered| &**registered.definition())
    }
    fn header(&self, function: &FunctionId) -> Option<std::sync::Arc<crate::FunctionHeader>> {
        self.registered(function)
            .map(|registered| std::sync::Arc::clone(registered.header()))
    }
    fn registered(&self, function: &FunctionId) -> Option<&crate::RegisteredFunction> {
        BuildHelpers::registered(self, function)
    }
}

/// Why a document's build-library closure cannot be projected.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum LibraryRequirementRefusal {
    #[error("build helper {function} is unavailable: {reason}")]
    HelperUnavailable {
        reason: lash_kernel_value::HelperUnavailableReason,
        function: FunctionId,
    },
}

/// Project the kernel versions of the exact helper closure, without copying
/// helper imports into the authored document.
pub fn library_requirements(
    document: &mut crate::Document,
    catalog: &dyn crate::FunctionCatalog,
) -> Result<(), LibraryRequirementRefusal> {
    let mut kernels = BTreeSet::new();
    helper_closure(document, catalog, |_, header, _| {
        kernels.insert(header.kernel);
    })?;
    document.requires.kernels.extend(kernels);
    Ok(())
}

/// The helper operation declarations a Link must explicitly fill. Body-local
/// numeric slots never become authored imports or execution identities.
pub fn helper_imports(
    document: &crate::Document,
    catalog: &dyn crate::FunctionCatalog,
) -> Result<
    BTreeMap<lash_kernel_value::HelperOperationId, lash_kernel_value::Import>,
    LibraryRequirementRefusal,
> {
    let mut imports = BTreeMap::new();
    helper_closure(document, catalog, |_, _, required| {
        for import in required.values() {
            imports.insert(
                lash_kernel_value::HelperOperationId::of_import(import),
                import.clone(),
            );
        }
    })?;
    Ok(imports)
}

fn helper_closure(
    document: &crate::Document,
    catalog: &dyn crate::FunctionCatalog,
    mut visit: impl FnMut(
        FunctionId,
        &crate::FunctionHeader,
        &BTreeMap<lash_kernel_value::ImportSlot, lash_kernel_value::Import>,
    ),
) -> Result<(), LibraryRequirementRefusal> {
    let mut pending: Vec<_> = document.library.iter().copied().collect();
    let mut seen = BTreeSet::new();
    while let Some(function) = pending.pop() {
        if !seen.insert(function) {
            continue;
        }
        let missing = || LibraryRequirementRefusal::HelperUnavailable {
            reason: lash_kernel_value::HelperUnavailableReason::Unknown,
            function,
        };
        let header = catalog.header(&function).ok_or_else(missing)?;
        let (callees, imports) = if let Some(registered) = catalog.registered(&function) {
            (registered.callees().clone(), registered.imports().clone())
        } else {
            catalog
                .definition(&function)
                .ok_or_else(missing)?
                .body()
                .map(|body| {
                    (
                        body.functions.keys().copied().collect(),
                        body.imports.clone(),
                    )
                })
                .unwrap_or_default()
        };
        visit(function, &header, &imports);
        pending.extend(callees);
    }
    Ok(())
}

type DocumentCheck = std::sync::Arc<std::sync::OnceLock<Result<(), crate::Invalid>>>;

/// Admission results for shared immutable documents, memoized by the hosted
/// owner that outlives the starts it serves. A result belongs to one exact
/// helper universe and one live `Arc<Document>`: weak keys retain no code,
/// and a changed body is a different allocation.
#[derive(Debug, Default)]
pub struct DocumentChecks(
    std::sync::Mutex<
        std::collections::VecDeque<(
            lash_kernel_value::HelperLibraryId,
            std::sync::Weak<crate::Document>,
            DocumentCheck,
        )>,
    >,
);

impl DocumentChecks {
    /// [`crate::validate_document`] against `helpers`, once per live document.
    ///
    /// # Errors
    ///
    /// The document's validation refusal.
    pub fn check(
        &self,
        document: &std::sync::Arc<crate::Document>,
        helpers: &BuildHelpers,
    ) -> Result<(), crate::Invalid> {
        let identity = helpers.identity();
        let checked = {
            let mut checks = self
                .0
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            checks.retain(|(_, document, _)| document.strong_count() != 0);
            if let Some((_, _, checked)) = checks.iter().find(|(helpers, known, _)| {
                *helpers == identity
                    && known
                        .upgrade()
                        .is_some_and(|known| std::sync::Arc::ptr_eq(&known, document))
            }) {
                std::sync::Arc::clone(checked)
            } else {
                let checked = DocumentCheck::default();
                checks.push_back((
                    identity,
                    std::sync::Arc::downgrade(document),
                    std::sync::Arc::clone(&checked),
                ));
                while checks.len() > 16 {
                    checks.pop_front();
                }
                checked
            }
        };
        // The owner, helpers and document are fixed before expensive work.
        // One OnceLock performs validation outside the inventory lock.
        checked
            .get_or_init(|| crate::validate_document(document, helpers))
            .clone()
    }
}
