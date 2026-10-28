//! Append-only release data. This module never installs code in an engine.
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{ActKind, Action, Callee, Expr, FunctionDefinition, Node, Stmt};
use lash_kernel_value::{FunctionId, HelperLibraryId};
use serde::{Deserialize, Serialize};

/// Stable upgrade pairing key, scoped by its owning package. Never executable.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct HelperId(pub String);

/// The Rust implementation a retained native declaration needs.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeIdentity {
    pub package: String,
    pub name: String,
    pub native_version: u32,
}

/// A conservative observable contract, independent of body and charge identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub signature: lash_kernel_value::Signature,
    pub errors: BTreeSet<String>,
    pub guard: Option<lash_kernel_doc::Guard>,
    pub kernel: u32,
    pub call_form: String,
    /// Canonical operation requirements, including signatures, across static reach.
    pub effects: BTreeSet<String>,
    /// Dynamic calls may invoke arbitrary caller code, including its operations.
    pub callback_effects: String,
    /// Reviewed representation guarantees, including their source owners.
    pub representations: BTreeMap<String, String>,
}

impl Contract {
    pub fn revision(&self) -> FunctionId {
        FunctionId::from_bytes(lash_kernel_value::tokens::logical_digest(
            &serde_json::to_vec(self)
                .unwrap_or_else(|error| unreachable!("contract fields serialize: {error}")),
        ))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContractRevision {
    pub revision: FunctionId,
    pub contract: Contract,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerEntry {
    pub revisions: Vec<ContractRevision>,
    /// Release sequence numbers at which the name was removed. Never erased.
    pub tombstones: Vec<usize>,
}

pub type Ledger = BTreeMap<HelperId, LedgerEntry>;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub revision: FunctionId,
    pub function: FunctionId,
}

/// One package's complete closed library, including required packages.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Library {
    pub identity: HelperLibraryId,
    pub members: BTreeSet<FunctionId>,
    pub roots: BTreeSet<FunctionId>,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            identity: HelperLibraryId::of_members([]),
            members: BTreeSet::new(),
            roots: BTreeSet::new(),
        }
    }
}

/// One immutable release delta. Unchanged definitions occur only in earlier deltas.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Release {
    /// The installed release identity and its exact closed universe.
    pub library: Library,
    pub definitions: BTreeMap<FunctionId, FunctionDefinition>,
    pub libraries: BTreeMap<String, Library>,
    pub bindings: BTreeMap<HelperId, Binding>,
    /// Exact declarations backed by Rust, distinguished from plan/body fallbacks.
    pub natives: BTreeMap<FunctionId, NativeIdentity>,
    /// Labels belong to the exact definitions, including historical ones.
    pub labels: BTreeMap<FunctionId, serde_json::Value>,
    pub representations: BTreeMap<String, String>,
}

/// Checked-in release deltas and a permanent contract history.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Archive {
    pub releases: Vec<Release>,
    pub ledger: Ledger,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveError {
    #[error("archive member {0} does not authenticate")]
    Hash(FunctionId),
    #[error("archive repeats member {0}")]
    Duplicate(FunctionId),
    #[error("archive library is not authenticated and closed: {0}")]
    Closure(String),
    #[error("helper contract history is invalid: {0:?}")]
    Contract(HelperId),
    #[error("archived native implementation is unavailable: {0:?}")]
    Native(NativeIdentity),
    #[error("release data or contract history was rewritten")]
    Rewritten,
}

type Definitions = BTreeMap<FunctionId, FunctionDefinition>;

fn reach(roots: &BTreeSet<FunctionId>, definitions: &Definitions) -> Option<BTreeSet<FunctionId>> {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<_> = roots.iter().copied().collect();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let definition = definitions.get(&id)?;
        if let Some(body) = definition.body() {
            for (callee, name) in &body.functions {
                if definitions.get(callee)?.name != *name {
                    return None;
                }
                pending.push(*callee);
            }
        }
    }
    Some(seen)
}

/// Derive header facts and transitive operation/callback effects from exact code.
/// The callback summary is deliberately conservative, including escaped closures.
pub fn contract(
    function: FunctionId,
    definitions: &Definitions,
    representations: &BTreeMap<String, String>,
) -> Option<Contract> {
    let definition = definitions.get(&function)?;
    let reached = reach(&BTreeSet::from([function]), definitions)?;
    let mut effects = BTreeSet::new();
    let mut callback = false;
    for id in reached {
        if let Some(body) = definitions[&id].body() {
            for requirement in body.imports.values() {
                effects.insert(serde_json::to_string(requirement).ok()?);
            }
            let mut pending = vec![Node::Block(&body.block)];
            while let Some(node) = pending.pop() {
                let kind = match node {
                    Node::Action(Action::Call {
                        callee: Callee::Value(_),
                        ..
                    })
                    | Node::Action(Action::Spawn {
                        callee: Callee::Value(_),
                        ..
                    }) => {
                        callback = true;
                        if matches!(node, Node::Action(Action::Spawn { .. })) {
                            Some("spawn")
                        } else {
                            None
                        }
                    }
                    Node::Expr(Expr::Act(act)) => match &act.kind {
                        ActKind::Call(Callee::Value(_)) | ActKind::Spawn(Callee::Value(_)) => {
                            callback = true;
                            if matches!(act.kind, ActKind::Spawn(_)) {
                                Some("spawn")
                            } else {
                                None
                            }
                        }
                        ActKind::Spawn(_) => Some("spawn"),
                        ActKind::Join | ActKind::JoinMany(_) => Some("join"),
                        ActKind::Await(_) => Some("await"),
                        ActKind::Yield => Some("yield"),
                        ActKind::Cancel => Some("cancel"),
                        _ => None,
                    },
                    Node::Action(Action::Spawn { .. }) => Some("spawn"),
                    Node::Action(Action::Join { .. } | Action::JoinMany { .. }) => Some("join"),
                    Node::Action(Action::Await { .. }) => Some("await"),
                    Node::Action(Action::Yield) => Some("yield"),
                    Node::Action(Action::Cancel { .. }) => Some("cancel"),
                    Node::Stmt(Stmt::Assign { .. } | Stmt::Remove { .. }) => Some("mutation"),
                    _ => None,
                };
                if let Some(kind) = kind {
                    effects.insert(kind.to_owned());
                }
                node.children_into(&mut pending);
            }
        }
    }
    Some(Contract {
        signature: definition.signature.clone(),
        errors: definition.errors.clone(),
        guard: definition.guard.clone(),
        kernel: definition.kernel,
        call_form: if definition.has_native() {
            "expression_or_action"
        } else {
            "action"
        }
        .to_owned(),
        effects,
        callback_effects: if callback {
            "unrestricted_caller_effects"
        } else {
            "none"
        }
        .to_owned(),
        representations: representations.clone(),
    })
}

impl Archive {
    pub(crate) fn members(&self, packages: &BTreeSet<String>) -> BTreeSet<FunctionId> {
        self.releases
            .iter()
            .flat_map(|release| {
                release
                    .libraries
                    .iter()
                    .filter(|(package, _)| packages.contains(*package))
                    .flat_map(|(_, library)| library.members.iter().copied())
            })
            .collect()
    }

    /// Earliest containing generation and package coverage for each installed member.
    /// The first release identifies the append-only lineage, independently of features.
    pub(crate) fn claim_generations(
        &self,
        members: &BTreeSet<FunctionId>,
    ) -> Result<BTreeMap<FunctionId, lash_kernel_value::claims::HelperGeneration>, ArchiveError>
    {
        let Some(first) = self.releases.first() else {
            return Ok(BTreeMap::new());
        };
        let mut packages = BTreeMap::new();
        let mut generations = BTreeMap::new();
        for (sequence, release) in self.releases.iter().enumerate() {
            let generation = u32::try_from(sequence + 1)
                .map_err(|_| ArchiveError::Closure("archive generation exceeds u32".to_owned()))?;
            for (name, library) in &release.libraries {
                packages.entry(name.clone()).or_insert(library.identity);
            }
            for function in release
                .definitions
                .keys()
                .filter(|function| members.contains(function))
            {
                let (name, _) = release
                    .libraries
                    .iter()
                    .filter(|(_, library)| library.members.contains(function))
                    .min_by_key(|(name, library)| (library.members.len(), *name))
                    .ok_or_else(|| ArchiveError::Closure(format!("no package owns {function}")))?;
                generations.entry(*function).or_insert(
                    lash_kernel_value::claims::HelperGeneration {
                        archive: first.library.identity,
                        generation,
                        package: packages[name],
                    },
                );
            }
        }
        Ok(generations)
    }

    /// Old binary feature combinations and package manifests whose complete
    /// membership is locally available. Exact installed identity stays separate.
    pub(crate) fn library_identities(
        &self,
        members: &BTreeSet<FunctionId>,
    ) -> BTreeSet<HelperLibraryId> {
        let mut identities = BTreeSet::new();
        for release in &self.releases {
            for library in std::iter::once(&release.library).chain(release.libraries.values()) {
                if library.members.is_subset(members) {
                    identities.insert(library.identity);
                }
            }
            // The four combinations of the shipped dialect features. Package
            // libraries include their dependencies; every binary also holds kernel.
            let shapes: [&[&str]; 4] = [
                &["kernel"],
                &["kernel", "python"],
                &[
                    "kernel",
                    "regex.ecma",
                    "date.ecma",
                    "url.whatwg",
                    "typescript",
                ],
                &[
                    "kernel",
                    "regex.ecma",
                    "date.ecma",
                    "url.whatwg",
                    "typescript",
                    "python",
                ],
            ];
            for shape in shapes {
                let Some(libraries) = shape
                    .iter()
                    .map(|package| release.libraries.get(*package))
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                let reached: BTreeSet<_> = libraries
                    .into_iter()
                    .flat_map(|library| library.members.iter().copied())
                    .collect();
                if reached.is_subset(members) {
                    identities.insert(HelperLibraryId::of_members(reached));
                }
            }
        }
        identities
    }

    /// Authenticate the entire archive before installing any historical member.
    /// Only explicitly installed packages contribute executable definitions.
    pub fn install(
        &self,
        mut registry: lash_kernel_doc::FunctionRegistry,
        packages: &BTreeSet<String>,
    ) -> Result<lash_kernel_doc::FunctionRegistry, ArchiveError> {
        let mut definitions = self.validate()?;
        let members = self.members(packages);
        definitions.retain(|id, _| members.contains(id) && registry.get(id).is_none());
        while !definitions.is_empty() {
            let next = definitions
                .iter()
                .find(|(_, definition)| {
                    definition.body().is_none_or(|body| {
                        body.functions.keys().all(|id| registry.get(id).is_some())
                    })
                })
                .map(|(id, _)| *id)
                .ok_or_else(|| ArchiveError::Closure("cyclic archive".to_owned()))?;
            let definition = definitions.remove(&next).ok_or(ArchiveError::Hash(next))?;
            registry
                .register(definition, None)
                .map_err(|error| ArchiveError::Closure(error.to_string()))?;
        }
        Ok(registry)
    }

    /// Authenticate every definition before following any of its edges, then
    /// check each historical closed library and its full-ledger contract binding.
    pub fn validate(&self) -> Result<Definitions, ArchiveError> {
        let mut definitions = Definitions::new();
        let mut known = BTreeMap::<HelperId, Vec<FunctionId>>::new();
        let mut previous = BTreeSet::new();
        let mut removed = BTreeMap::<HelperId, Vec<usize>>::new();
        for (sequence, release) in self.releases.iter().enumerate() {
            for (id, definition) in &release.definitions {
                if definition.identity() != *id {
                    return Err(ArchiveError::Hash(*id));
                }
                if definitions.insert(*id, definition.clone()).is_some() {
                    return Err(ArchiveError::Duplicate(*id));
                }
            }
            for id in release.definitions.keys() {
                if let Some(body) = definitions[id].body()
                    && Node::Block(&body.block)
                        .functions_within()
                        .iter()
                        .any(|callee| !body.functions.contains_key(callee))
                {
                    return Err(ArchiveError::Closure(format!("definition {id}")));
                }
                if reach(&BTreeSet::from([*id]), &definitions).is_none() {
                    return Err(ArchiveError::Closure(format!("definition {id}")));
                }
            }
            for (package, library) in &release.libraries {
                if HelperLibraryId::of_members(library.members.iter().copied()) != library.identity
                    || reach(&library.roots, &definitions).as_ref() != Some(&library.members)
                {
                    return Err(ArchiveError::Closure(package.clone()));
                }
            }
            let members = release
                .libraries
                .values()
                .flat_map(|library| library.members.iter().copied())
                .collect();
            let roots = release
                .libraries
                .values()
                .flat_map(|library| library.roots.iter().copied())
                .collect();
            if release.library.members != members
                || release.library.roots != roots
                || release.library.identity
                    != HelperLibraryId::of_members(release.library.members.iter().copied())
            {
                return Err(ArchiveError::Closure("release".to_owned()));
            }
            for (function, native) in &release.natives {
                let definition = definitions
                    .get(function)
                    .ok_or_else(|| ArchiveError::Native(native.clone()))?;
                if !definition.has_native()
                    || definition.name.as_str() != native.name
                    || definition.native_version != native.native_version
                    || release
                        .libraries
                        .get(&native.package)
                        .is_none_or(|library| !library.roots.contains(function))
                {
                    return Err(ArchiveError::Native(native.clone()));
                }
            }
            for function in &release.library.members {
                if matches!(
                    definitions[function].implementation,
                    lash_kernel_doc::Implementation::Native { .. }
                ) && !release.natives.contains_key(function)
                {
                    return Err(ArchiveError::Closure(format!(
                        "native-only {function} has no Rust binding"
                    )));
                }
            }
            let present: BTreeSet<_> = release.bindings.keys().cloned().collect();
            for helper in previous.difference(&present) {
                removed.entry(helper.clone()).or_default().push(sequence);
            }
            for (helper, binding) in &release.bindings {
                let (package, name) = helper
                    .0
                    .split_once(':')
                    .ok_or_else(|| ArchiveError::Contract(helper.clone()))?;
                let definition = definitions
                    .get(&binding.function)
                    .ok_or_else(|| ArchiveError::Contract(helper.clone()))?;
                let library = release
                    .libraries
                    .get(package)
                    .ok_or_else(|| ArchiveError::Contract(helper.clone()))?;
                let derived = contract(binding.function, &definitions, &release.representations)
                    .ok_or_else(|| ArchiveError::Contract(helper.clone()))?;
                let entry = self
                    .ledger
                    .get(helper)
                    .ok_or_else(|| ArchiveError::Contract(helper.clone()))?;
                if name == "kernel.anonymous"
                    || name != definition.name.as_str()
                    || !library.roots.contains(&binding.function)
                    || derived.revision() != binding.revision
                    || !entry.revisions.iter().any(|revision| {
                        revision.revision == binding.revision && revision.contract == derived
                    })
                {
                    return Err(ArchiveError::Contract(helper.clone()));
                }
                let revisions = known.entry(helper.clone()).or_default();
                if removed.contains_key(helper) && revisions.last() != Some(&binding.revision) {
                    return Err(ArchiveError::Contract(helper.clone()));
                }
                if !revisions.contains(&binding.revision) {
                    revisions.push(binding.revision);
                }
            }
            // Every public root has exactly one package-qualified pairing key.
            for (package, library) in &release.libraries {
                for root in &library.roots {
                    let name = definitions[root].name.as_str();
                    if name != "kernel.anonymous"
                        && release
                            .bindings
                            .get(&HelperId(format!("{package}:{name}")))
                            .is_none_or(|binding| binding.function != *root)
                    {
                        return Err(ArchiveError::Closure(package.clone()));
                    }
                }
            }
            previous = present;
        }
        for (helper, entry) in &self.ledger {
            if known.get(helper) != Some(&entry.revisions.iter().map(|r| r.revision).collect())
                || entry
                    .revisions
                    .iter()
                    .any(|r| r.contract.revision() != r.revision)
                || removed.get(helper).cloned().unwrap_or_default() != entry.tombstones
            {
                return Err(ArchiveError::Contract(helper.clone()));
            }
        }
        Ok(definitions)
    }

    /// Compare against the previously released tree before accepting an append.
    pub fn check_append(&self, next: &Self) -> Result<(), ArchiveError> {
        if !next.releases.starts_with(&self.releases) {
            return Err(ArchiveError::Rewritten);
        }
        for (helper, entry) in &self.ledger {
            let Some(new) = next.ledger.get(helper) else {
                return Err(ArchiveError::Rewritten);
            };
            if !new.revisions.starts_with(&entry.revisions)
                || !new.tombstones.starts_with(&entry.tombstones)
            {
                return Err(ArchiveError::Rewritten);
            }
        }
        let _ = next.validate()?;
        Ok(())
    }

    /// The complete native support boundary, by implementation identity rather
    /// than FunctionId. Deleting a retained version must fail release checks.
    pub fn check_natives(&self, available: &BTreeSet<NativeIdentity>) -> Result<(), ArchiveError> {
        let _ = self.validate()?;
        for release in &self.releases {
            for identity in release.natives.values() {
                if !available.contains(identity) {
                    return Err(ArchiveError::Native(identity.clone()));
                }
            }
        }
        Ok(())
    }

    /// Append a build's full release, deduplicating exact definitions. A recorder
    /// explicitly commits the emitted candidate; ordinary builds write OUT_DIR only.
    pub fn append(&self, mut release: Release) -> Result<Self, ArchiveError> {
        let definitions = self.validate()?;
        release
            .definitions
            .retain(|id, _| !definitions.contains_key(id));
        if self.releases.last().is_some_and(|last| {
            last.library == release.library
                && last.libraries == release.libraries
                && last.bindings == release.bindings
                && last.natives == release.natives
                && last.labels == release.labels
                && last.representations == release.representations
        }) {
            return Ok(self.clone());
        }
        let mut next = self.clone();
        let all: Definitions = definitions
            .into_iter()
            .chain(release.definitions.clone())
            .collect();
        let previous: BTreeSet<_> = self
            .releases
            .last()
            .into_iter()
            .flat_map(|r| r.bindings.keys().cloned())
            .collect();
        for helper in previous.difference(&release.bindings.keys().cloned().collect()) {
            next.ledger
                .entry(helper.clone())
                .or_default()
                .tombstones
                .push(self.releases.len());
        }
        for (helper, binding) in &release.bindings {
            let derived = contract(binding.function, &all, &release.representations)
                .ok_or_else(|| ArchiveError::Contract(helper.clone()))?;
            let entry = next.ledger.entry(helper.clone()).or_default();
            if !entry
                .revisions
                .iter()
                .any(|r| r.revision == binding.revision)
            {
                entry.revisions.push(ContractRevision {
                    revision: binding.revision,
                    contract: derived,
                });
            }
        }
        next.releases.push(release);
        self.check_append(&next)?;
        Ok(next)
    }
}
