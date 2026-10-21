//! One package artifact: its path, the exact artifacts it was built
//! against, the roots its definitions live under, its exports and its code.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use lash_helper_labels::MemberDisplay;
use lash_kernel_dialect::Export;
use lash_kernel_doc::ArtifactDigest;
use lash_kernel_doc::FunctionDefinition;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::NativeFunction;
use lash_kernel_doc::ReleasedFunctions;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::FunctionName;
use lash_vocabulary::names::MemberPath;
use lash_vocabulary::names::PackagePath;
use lash_vocabulary::names::Segment;
use serde::{Deserialize, Serialize};

use crate::PackageRefusal;

/// The content identity of a package artifact.
/// version_surface = "coexist"
/// version_guard(items(PACKAGE_ARTIFACT_DOMAIN, Package))
const PACKAGE_ARTIFACT_DOMAIN: &str = "lash package artifact 2026-10 v1";

/// An artifact's key: its line and its digest. Two artifacts of one line
/// (an own release and a retained one) are two keys.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ArtifactKey {
    pub path: PackagePath,
    pub digest: ArtifactDigest,
}

impl fmt::Display for ArtifactKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.path, self.digest.short())
    }
}

/// A prefix of definition names a package's code lives under: `ts`,
/// `regex.ecma`, `dataframe`. Definition names are not canonical names, so
/// a root is plain dotted text.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Root(String);

impl Root {
    pub fn new(text: impl Into<String>) -> Self {
        Self(text.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether the definition `name` sits under this root.
    pub fn holds(&self, name: &str) -> bool {
        name == self.0
            || name
                .strip_prefix(self.0.as_str())
                .is_some_and(|rest| rest.starts_with('.'))
    }

    /// Whether a namespace at `path` would meet this root: one is a prefix
    /// of the other, segment by segment.
    pub fn meets(&self, path: &PackagePath) -> bool {
        let path = path.to_string();
        self.holds(&path) || Root(path).holds(&self.0)
    }
}

impl fmt::Display for Root {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Who built an artifact: lash itself, or a host.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// One of lash's own code-only packages. Its roots are reserved.
    Lash,
    /// A host's package. Its one root is its own path.
    Host,
}

/// Labels for a package's members and private definitions that back no member. Display data never
/// enters the digest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDisplay {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub members: BTreeMap<MemberPath, MemberDisplay>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub definitions: BTreeMap<FunctionName, MemberDisplay>,
}

impl PackageDisplay {
    /// Whether it labels nothing.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty() && self.definitions.is_empty()
    }
}

/// A package's code: its definitions in the order they register.
#[derive(Clone, Debug)]
pub struct Code {
    ids: Vec<FunctionId>,
    store: CodeStore,
}

#[derive(Clone, Debug)]
enum CodeStore {
    /// Definitions made in this process, parallel to `ids`, each after
    /// the functions it calls.
    Defined(Vec<Arc<FunctionDefinition>>),
    /// A build's release image, registered in place; `ids` are in
    /// identity order.
    Image(ReleasedFunctions),
    Registry(Arc<FunctionRegistry>),
}

impl Code {
    pub(crate) fn defined(definitions: Vec<(FunctionId, Arc<FunctionDefinition>)>) -> Self {
        let (ids, definitions) = definitions.into_iter().unzip();
        Self {
            ids,
            store: CodeStore::Defined(definitions),
        }
    }

    pub(crate) fn image(image: ReleasedFunctions) -> Self {
        Self {
            ids: image.ids().collect(),
            store: CodeStore::Image(image),
        }
    }

    /// Every function, in registration order.
    pub fn ids(&self) -> &[FunctionId] {
        &self.ids
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// The `at`th function's definition name.
    pub fn name(&self, at: usize) -> &str {
        match &self.store {
            CodeStore::Defined(definitions) => definitions[at].name.as_str(),
            CodeStore::Image(image) => image.name(at),
            CodeStore::Registry(registry) => registry
                .name_of(&self.ids[at])
                .unwrap_or_else(|| unreachable!("frame membership")),
        }
    }

    /// The `at`th function's definition, decoded from an image when read.
    ///
    /// # Errors
    ///
    /// An image's decoding error, rendered: a defect of the build.
    pub fn definition(&self, at: usize) -> Result<Arc<FunctionDefinition>, String> {
        match &self.store {
            CodeStore::Defined(definitions) => Ok(Arc::clone(&definitions[at])),
            CodeStore::Registry(registry) => registry
                .get(&self.ids[at])
                .map(|registered| registered.definition().clone())
                .ok_or_else(|| "missing frame function".into()),
            CodeStore::Image(image) => image
                .definition(at)
                .map(Arc::new)
                .map_err(|error| error.to_string()),
        }
    }

    /// Where the function `name` stands, when the code defines one.
    pub fn position(&self, name: &str) -> Option<usize> {
        match &self.store {
            CodeStore::Registry(registry) => registry
                .named(name)
                .filter_map(|function| self.ids.iter().position(|held| *held == function))
                // Several retained identities may share a name. Preserve
                // this artifact's first matching position.
                .min(),
            _ => (0..self.len()).find(|at| self.name(*at) == name),
        }
    }

    /// Registers every function `registry` does not hold yet, each with
    /// the native `native` gives it.
    pub fn register_into(
        &self,
        registry: &mut FunctionRegistry,
        native: &dyn Fn(&FunctionId) -> Option<Arc<dyn NativeFunction>>,
    ) -> Result<(), String> {
        let missing: Vec<usize> = (0..self.len())
            .filter(|at| registry.get(&self.ids[*at]).is_none())
            .collect();
        if missing.is_empty() {
            return Ok(());
        }
        if let CodeStore::Image(image) = &self.store
            && missing.len() == self.len()
        {
            registry
                .register_released(*image)
                .map_err(|error| error.to_string())?;
            for function in &self.ids {
                if let Some(native) = native(function) {
                    registry
                        .implement(*function, native)
                        .map_err(|error| error.to_string())?;
                }
            }
            return Ok(());
        }
        for at in dependency_order(self, &missing)? {
            let definition = FunctionDefinition::clone(self.definition(at)?.as_ref());
            registry
                .register(definition, native(&self.ids[at]))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }
}

/// `missing` reordered so each function follows those it calls.
fn dependency_order(code: &Code, missing: &[usize]) -> Result<Vec<usize>, String> {
    let at: BTreeMap<FunctionId, usize> = missing.iter().map(|at| (code.ids[*at], *at)).collect();
    let mut order = Vec::with_capacity(missing.len());
    let mut placed = BTreeSet::new();
    fn place(
        code: &Code,
        index: usize,
        at: &BTreeMap<FunctionId, usize>,
        placed: &mut BTreeSet<usize>,
        order: &mut Vec<usize>,
    ) -> Result<(), String> {
        if !placed.insert(index) {
            return Ok(());
        }
        if let Some(body) = code.definition(index)?.body() {
            for callee in body.functions.keys() {
                if let Some(callee) = at.get(callee) {
                    place(code, *callee, at, placed, order)?;
                }
            }
        }
        order.push(index);
        Ok(())
    }
    for index in missing {
        place(code, *index, &at, &mut placed, &mut order)?;
    }
    Ok(order)
}

/// One package artifact.
#[derive(Clone, Debug)]
pub struct Package {
    pub(crate) path: PackagePath,
    pub(crate) release: String,
    pub(crate) origin: Origin,
    pub(crate) requires: BTreeMap<PackagePath, ArtifactDigest>,
    pub(crate) roots: BTreeSet<Root>,
    pub(crate) exports: BTreeMap<Segment, Export>,
    pub(crate) code: Code,
    pub(crate) display: PackageDisplay,
    /// Computed from the canonical facts when the artifact is made; never
    /// written into them.
    pub(crate) digest: ArtifactDigest,
}

/// What an artifact is besides its code: what a build writes beside its
/// image, and every fact its digest covers but the code.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageHeader {
    pub path: PackagePath,
    /// A label for display and messages, never identity.
    pub release: String,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requires: BTreeMap<PackagePath, ArtifactDigest>,
    pub roots: BTreeSet<Root>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub exports: BTreeMap<Segment, Export>,
}

impl Package {
    pub fn path(&self) -> &PackagePath {
        &self.path
    }

    /// The host's release label: display and messages only.
    pub fn release(&self) -> &str {
        &self.release
    }

    pub fn origin(&self) -> Origin {
        self.origin
    }

    /// The exact artifact of each line its bodies were built against.
    pub fn requires(&self) -> &BTreeMap<PackagePath, ArtifactDigest> {
        &self.requires
    }

    pub fn roots(&self) -> &BTreeSet<Root> {
        &self.roots
    }

    pub fn exports(&self) -> &BTreeMap<Segment, Export> {
        &self.exports
    }

    pub fn code(&self) -> &Code {
        &self.code
    }

    pub fn display(&self) -> &PackageDisplay {
        &self.display
    }

    pub fn digest(&self) -> ArtifactDigest {
        self.digest
    }

    pub fn key(&self) -> ArtifactKey {
        ArtifactKey {
            path: self.path.clone(),
            digest: self.digest,
        }
    }

    /// The kernel error kind of the export `segment` when it is an error:
    /// `<path>.<segment>`, derived and never written.
    pub fn error_kind(&self, segment: &Segment) -> Option<String> {
        matches!(self.exports.get(segment), Some(Export::Error))
            .then(|| format!("{}.{segment}", self.path))
    }

    /// The identity of the function this artifact defines as `name`.
    pub fn function(&self, name: &str) -> Option<FunctionId> {
        self.code.position(name).map(|at| self.code.ids()[at])
    }

    /// This artifact with `display` as its display data, which no digest
    /// covers.
    #[must_use]
    pub fn with_display(mut self, display: PackageDisplay) -> Self {
        self.display = display;
        self
    }

    /// The facts a build writes beside the code's image.
    pub fn header(&self) -> PackageHeader {
        PackageHeader {
            path: self.path.clone(),
            release: self.release.clone(),
            origin: self.origin,
            requires: self.requires.clone(),
            roots: self.roots.clone(),
            exports: self.exports.clone(),
        }
    }

    /// The artifact a build wrote as `header` and the release `image` of
    /// its code. Its digest is computed again, never read: from its code,
    /// or, for an artifact holding a building-artifact function, the one
    /// its build gave that function.
    ///
    /// # Errors
    ///
    /// [`PackageRefusal::Source`] when the image holds a building-artifact
    /// function that does not return a digest.
    pub fn from_image(
        header: PackageHeader,
        image: ReleasedFunctions,
    ) -> Result<Self, PackageRefusal> {
        Self::from_code(header, Code::image(image))
    }

    /// Reconstructs an artifact over the one shared registry image. Membership
    /// is checked by identity; no second copy of its definitions is registered.
    pub fn from_registry(
        header: PackageHeader,
        functions: Vec<FunctionId>,
        registry: Arc<FunctionRegistry>,
    ) -> Result<Self, PackageRefusal> {
        for function in &functions {
            if registry.position(function).is_none() {
                return Err(PackageRefusal::Source {
                    package: header.path.clone(),
                    message: format!("frame lacks {function}"),
                });
            }
        }
        Self::from_code(
            header,
            Code {
                ids: functions,
                store: CodeStore::Registry(registry),
            },
        )
    }

    fn from_code(header: PackageHeader, code: Code) -> Result<Self, PackageRefusal> {
        let PackageHeader {
            path,
            release,
            origin,
            requires,
            roots,
            exports,
        } = header;
        let digest = digest_of(&path, origin, &requires, &roots, &exports, code.ids());
        Ok(Self {
            path,
            release,
            origin,
            requires,
            roots,
            exports,
            code,
            display: PackageDisplay::default(),
            digest,
        })
    }
}

/// The blake3 digest of an artifact's canonical facts: its path, origin,
/// requirements, roots, exports and the identities of its code. The
/// release label and display data are not identity.
pub(crate) fn digest_of(
    path: &PackagePath,
    origin: Origin,
    requires: &BTreeMap<PackagePath, ArtifactDigest>,
    roots: &BTreeSet<Root>,
    exports: &BTreeMap<Segment, Export>,
    code: &[FunctionId],
) -> ArtifactDigest {
    let mut hasher = blake3::Hasher::new_derive_key(PACKAGE_ARTIFACT_DOMAIN);
    let mut field = |bytes: &[u8]| {
        hasher.update(&(bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    };
    field(path.to_string().as_bytes());
    field(match origin {
        Origin::Lash => b"lash",
        Origin::Host => b"host",
    });
    field(&(requires.len() as u64).to_le_bytes());
    for (line, digest) in requires {
        field(line.to_string().as_bytes());
        field(digest.as_bytes());
    }
    field(&(roots.len() as u64).to_le_bytes());
    for root in roots {
        field(root.as_str().as_bytes());
    }
    field(
        &serde_json::to_vec(exports)
            .unwrap_or_else(|_| unreachable!("exports serialize: string keys, no floats")),
    );
    let mut ids: Vec<&FunctionId> = code.iter().collect();
    ids.sort_unstable();
    field(&(ids.len() as u64).to_le_bytes());
    for id in ids {
        field(id.as_bytes());
    }
    ArtifactDigest::from_bytes(*hasher.finalize().as_bytes())
}
