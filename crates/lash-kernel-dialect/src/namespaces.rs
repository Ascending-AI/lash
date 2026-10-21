//! The operations a document is offered, and the one namespace tree its
//! dotted names resolve through (§2.3).
//!
//! Every operation a host offers a cell or a workflow (a catalog tool, a
//! deferred grant, an unavailable note) is one entry of [`Operations`]. A
//! front end resolves a written dotted path once, through
//! [`Namespaces::resolve`], after its own lexical bindings and globals: a
//! package path walked segment by segment, each segment unspelled by the
//! dialect's [`Spelling`]. Nothing here reads a dialect.

use std::collections::{BTreeMap, BTreeSet};

use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::operations::OperationRequirement;
use serde::{Deserialize, Serialize};

use crate::EffectControl;
use crate::Export;
use crate::ExportView;
use crate::SpellKind;
use crate::Spelling;
use crate::TypeMember;
use lash_vocabulary::names::PackagePath;
use lash_vocabulary::names::QualifiedOperation;
use lash_vocabulary::names::Segment;

/// What a cell or workflow is offered under one operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Offered {
    /// What a perform of it states: its signature and contract.
    pub requirement: OperationRequirement,
    /// The turn-ending controls its result may be; empty for most.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub controls: BTreeSet<EffectControl>,
    /// Whether the host performs it as a read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read: bool,
    /// The declaring package's visibility, projected from this cell's view.
    #[serde(skip)]
    pub visibility: crate::Visibility,
    pub source: OfferedSource,
}

impl Offered {
    /// An operation the catalog tool `tool` serves under `requirement`, the
    /// tool's signature and the contract it serves: no controls, not a read.
    #[must_use]
    pub fn tool(tool: impl Into<String>, requirement: OperationRequirement) -> Self {
        Self {
            requirement,
            controls: BTreeSet::new(),
            read: false,
            source: OfferedSource::Catalog { tool: tool.into() },
            visibility: crate::Visibility::Public,
        }
    }

    /// This operation, whose result may be one of `controls`.
    #[must_use]
    pub fn with_controls(mut self, controls: BTreeSet<EffectControl>) -> Self {
        self.controls = controls;
        self
    }
}

/// Where an offered operation comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum OfferedSource {
    /// A tool of the run's catalog, by its tool id.
    Catalog { tool: String },
    /// A tool the run's deferred provider granted this cell, by its tool id.
    Deferred { tool: String },
    /// A path the deferred provider answered with a note: a call raises it.
    Unavailable { note: String },
}

/// The operations offered, one entry per operation (P1).
pub type Operations = BTreeMap<QualifiedOperation, Offered>;

use lash_vocabulary::{operation_keys, operation_text};

/// `operation` as a dialect's source names it: its package path, then its
/// name, each in the dialect's spelling. The one path renderer lowering
/// repairs, printers and prompts share.
#[must_use]
pub fn spell_operation(spelling: Spelling, operation: &QualifiedOperation) -> String {
    let mut path = String::new();
    for segment in operation.package.iter() {
        path.push_str(&spelling.spell(SpellKind::PathSegment, segment));
        path.push('.');
    }
    path.push_str(&spelling.spell(SpellKind::Member, &operation.name));
    path
}

/// Why a namespace tree could not be composed: each names both sides.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum NamespaceRefusal {
    /// A package path is another package's export.
    #[error("the package path `{path}` is the export `{export}` of `{package}`")]
    PathIsExport {
        path: String,
        package: String,
        export: String,
    },
    /// An offered operation at a segment a code package exports as
    /// something other than an operation: the tool would shadow the code.
    #[error("the operation `{operation}` is the code export `{export}` of `{package}`")]
    OperationIsExport {
        operation: String,
        package: String,
        export: String,
    },
    /// An offered operation a package declares, offered under another
    /// contract than the package's.
    #[error(
        "the operation `{operation}` is offered under contract `{offered}`, and `{package}` declares `{declared}`"
    )]
    OperationContractDiffers {
        operation: String,
        package: String,
        offered: String,
        declared: String,
    },
    /// An offered effect name that is no canonical operation.
    #[error("`{effect}` is not an operation: {message}")]
    NotAnOperation { effect: String, message: String },
    /// An offered operation cannot add a member to an installed package.
    #[error("no such member `{operation}` is declared by installed package `{package}`")]
    UndeclaredOperation { operation: String, package: String },
    /// A package root no package may take.
    #[error("the package path `{path}` takes the root `{root}`, which is {by}")]
    ReservedRoot {
        path: String,
        root: String,
        by: String,
    },
}

/// What a written dotted path names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolution<'a> {
    /// An offered operation: a call of it is a `perform`.
    Operation {
        operation: &'a QualifiedOperation,
        offered: &'a Offered,
    },
    /// A code export, or a member of an exported type.
    Export {
        package: &'a PackagePath,
        segment: &'a Segment,
        export: &'a Export,
        member: Option<(&'a Segment, &'a TypeMember)>,
    },
    /// A package path or a prefix of one: a namespace, never a value.
    Namespace,
    /// Unknown here.
    Unknown,
}

/// A written path that names an offered operation in another dialect's
/// spelling, with the spelling this dialect writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Misspelled {
    pub operation: QualifiedOperation,
    pub repair: String,
}

/// The namespace tree of one environment: the offered operations and the
/// code exports of the packages it is composed from.
#[derive(Clone, Debug, Default)]
pub struct Namespaces {
    operations: Operations,
    code: BTreeMap<PackagePath, BTreeMap<Segment, Export>>,
    /// Every package path and each of its prefixes.
    nodes: BTreeSet<Vec<Segment>>,
}

/// The spelling kind an export is written in.
fn spelled_as(export: &Export) -> SpellKind {
    match export {
        Export::Type(_) | Export::Error => SpellKind::TypeName,
        Export::Operation(_) | Export::Function(_) => SpellKind::Member,
    }
}

fn dotted<'a>(segments: impl IntoIterator<Item = &'a Segment>) -> String {
    segments
        .into_iter()
        .map(Segment::as_str)
        .collect::<Vec<_>>()
        .join(".")
}

impl Namespaces {
    /// The tree of `view`'s code packages and `operations`.
    ///
    /// # Errors
    ///
    /// [`NamespaceRefusal`]: an offered operation at a code export that is
    /// no operation, or at an operation export under another contract than
    /// the package declares, a package path equal to another package's export, or
    /// a root that is reserved (`lash`, a dialect global, a
    /// printer-generated name, a root of lash's own packages).
    pub fn build(
        view: &dyn ExportView,
        mut operations: Operations,
    ) -> Result<Self, NamespaceRefusal> {
        let mut code = BTreeMap::new();
        for path in view.paths() {
            if let Some(exports) = view.exports(&path) {
                code.insert(path, exports.clone());
            }
        }
        let packages = code
            .keys()
            .cloned()
            .chain(operations.keys().map(|operation| operation.package.clone()))
            .collect::<BTreeSet<_>>();
        // What each node exports, by segment: its code exports and its
        // operations, named for a refusal.
        let mut exports = BTreeMap::<Vec<Segment>, BTreeMap<Segment, String>>::new();
        for (path, members) in &code {
            for segment in members.keys() {
                exports.entry(path.as_slice().to_vec()).or_default().insert(
                    segment.clone(),
                    format!("{}.{segment}", dotted(path.iter())),
                );
            }
        }
        for operation in operations.keys() {
            exports
                .entry(operation.package.as_slice().to_vec())
                .or_default()
                .insert(operation.name.clone(), operation.effect_name().to_string());
        }
        let mut nodes = BTreeSet::new();
        for package in &packages {
            let segments = package.as_slice();
            let root = segments[0].as_str();
            let reserved = (root == "lash")
                .then(|| "lash's own namespace".to_owned())
                .or_else(|| view.reserves(root));
            if let Some(by) = reserved {
                return Err(NamespaceRefusal::ReservedRoot {
                    path: dotted(segments),
                    root: root.to_owned(),
                    by,
                });
            }
            for end in 1..=segments.len() {
                let (parent, segment) = (&segments[..end - 1], &segments[end - 1]);
                if let Some(export) = exports.get(parent).and_then(|node| node.get(segment)) {
                    return Err(NamespaceRefusal::PathIsExport {
                        path: dotted(segments),
                        package: dotted(parent),
                        export: export.clone(),
                    });
                }
                nodes.insert(segments[..end].to_vec());
            }
        }
        // One path, one owner. An offered operation at a segment a code
        // package exports as anything but an operation would shadow the
        // code, so it is refused naming both. One the package declares is
        // performed under the package's declared requirement, the one its
        // own bodies state: a runner serves it under that contract
        // (PKG-RUNNER), so a direct perform and a member body's state one
        // requirement.
        for (operation, offered) in &mut operations {
            let mut path = operation.package.as_slice().to_vec();
            path.push(operation.name.clone());
            let class = crate::classify_package_path(view, &path);
            offered.visibility = if class == crate::PackagePathClass::Internal {
                crate::Visibility::Internal
            } else {
                crate::Visibility::Public
            };
            let Some(export) = code
                .get(&operation.package)
                .and_then(|members| members.get(&operation.name))
            else {
                if class != crate::PackagePathClass::Outside {
                    return Err(NamespaceRefusal::UndeclaredOperation {
                        operation: operation.effect_name().to_string(),
                        package: path[0].as_str().to_owned(),
                    });
                }
                continue;
            };
            let package = dotted(operation.package.iter());
            let Export::Operation(declared) = export else {
                return Err(NamespaceRefusal::OperationIsExport {
                    operation: operation.effect_name().to_string(),
                    export: format!("{package}.{}", operation.name),
                    package,
                });
            };
            if offered.requirement.contract != declared.requirement.contract {
                return Err(NamespaceRefusal::OperationContractDiffers {
                    operation: operation.effect_name().to_string(),
                    package,
                    offered: offered.requirement.contract.to_string(),
                    declared: declared.requirement.contract.to_string(),
                });
            }
            offered.requirement.clone_from(&declared.requirement);
        }
        Ok(Self {
            operations,
            code,
            nodes,
        })
    }

    /// The tree of `operations` alone, with no code package.
    ///
    /// # Errors
    ///
    /// As [`Self::build`].
    pub fn of_operations(operations: Operations) -> Result<Self, NamespaceRefusal> {
        Self::build(&crate::NoPackages, operations)
    }

    /// The tree of the operations `signatures` names, each served under
    /// `contract` by the catalog tool of its effect name, and those of
    /// `controls` declaring theirs: what an embedder that types and serves
    /// its effects itself, with no tool catalog, lowers against.
    ///
    /// # Errors
    ///
    /// As [`Self::of_requirements`].
    pub fn of_effects(
        signatures: &BTreeMap<EffectName, lash_vocabulary::types::Signature>,
        contract: &lash_vocabulary::operations::ContractId,
        controls: &BTreeMap<EffectName, BTreeSet<EffectControl>>,
    ) -> Result<Self, NamespaceRefusal> {
        let requirements = signatures
            .iter()
            .map(|(effect, signature)| {
                (
                    effect.clone(),
                    OperationRequirement::new(signature.clone(), contract.clone()),
                )
            })
            .collect();
        Self::of_requirements(&requirements, controls)
    }

    /// The tree of the operations `requirements` names, each offered under
    /// its requirement by the catalog tool of its effect name, and those of
    /// `controls` declaring theirs: what a document is read back against
    /// its own manifest.
    ///
    /// # Errors
    ///
    /// [`NamespaceRefusal::NotAnOperation`] for an effect name that is no
    /// canonical operation; otherwise as [`Self::build`].
    pub fn of_requirements(
        requirements: &BTreeMap<EffectName, OperationRequirement>,
        controls: &BTreeMap<EffectName, BTreeSet<EffectControl>>,
    ) -> Result<Self, NamespaceRefusal> {
        let mut operations = Operations::new();
        for (effect, requirement) in requirements {
            let operation = QualifiedOperation::from_effect(effect).map_err(|error| {
                NamespaceRefusal::NotAnOperation {
                    effect: effect.to_string(),
                    message: error.to_string(),
                }
            })?;
            let offered = Offered::tool(effect.as_str(), requirement.clone())
                .with_controls(controls.get(effect).cloned().unwrap_or_default());
            operations.insert(operation, offered);
        }
        Self::of_operations(operations)
    }

    /// The offered operations.
    #[must_use]
    pub fn operations(&self) -> &Operations {
        &self.operations
    }

    /// Each offered operation's effect name with its signature: what the
    /// checker admits a document against.
    #[must_use]
    pub fn signatures(&self) -> BTreeMap<EffectName, lash_vocabulary::types::Signature> {
        self.operations
            .iter()
            .map(|(operation, offered)| {
                (
                    operation.effect_name(),
                    offered.requirement.signature.clone(),
                )
            })
            .collect()
    }

    /// The operation offered under `effect`.
    #[must_use]
    pub fn operation(&self, effect: &EffectName) -> Option<(&QualifiedOperation, &Offered)> {
        let operation = QualifiedOperation::from_effect(effect).ok()?;
        self.operations.get_key_value(&operation)
    }

    /// The turn controls `effect` declares; empty for an effect that
    /// declares none or is not offered.
    #[must_use]
    pub fn controls(&self, effect: &EffectName) -> Option<&BTreeSet<EffectControl>> {
        self.operation(effect)
            .map(|(_, offered)| &offered.controls)
            .filter(|controls| !controls.is_empty())
    }

    /// What `written`, a dotted path in `spelling`, names.
    #[must_use]
    pub fn resolve(&self, written: &[&str], spelling: Spelling) -> Resolution<'_> {
        if let Some(resolved) = self.resolve_operation(written, spelling) {
            return resolved;
        }
        // Only the package path is spelled as path segments: an export
        // or a member after it is spelled by its own kind (`DataQuanta`).
        let segments = written
            .iter()
            .map(|segment| spelling.unspell(SpellKind::PathSegment, segment))
            .collect::<Vec<_>>();
        for end in 1..written.len() {
            let Some(package) = segments[..end].iter().cloned().collect::<Option<Vec<_>>>() else {
                break;
            };
            let Ok(path) = PackagePath::try_from(package) else {
                continue;
            };
            let Some(exports) = self.code.get(&path) else {
                continue;
            };
            let (package, _) = self
                .code
                .get_key_value(&path)
                .unwrap_or_else(|| unreachable!("the path was just found"));
            let rest = &written[end..];
            // An export is written in its own kind's spelling only: a
            // function as a member, a type or an error as a type name.
            let found = [SpellKind::Member, SpellKind::TypeName]
                .into_iter()
                .filter_map(|kind| Some((kind, spelling.unspell(kind, rest[0])?)))
                .filter_map(|(kind, segment)| Some((kind, exports.get_key_value(&segment)?)))
                .find_map(|(kind, found)| (spelled_as(found.1) == kind).then_some(found));
            let Some((segment, export)) = found else {
                continue;
            };
            let member = match (export, rest.get(1)) {
                (_, None) => None,
                (Export::Type(value_type), Some(member)) if rest.len() == 2 => {
                    let Some(found) = spelling
                        .unspell(SpellKind::Member, member)
                        .and_then(|member| value_type.members.get_key_value(&member))
                    else {
                        return Resolution::Unknown;
                    };
                    Some(found)
                }
                _ => return Resolution::Unknown,
            };
            return Resolution::Export {
                package,
                segment,
                export,
                member,
            };
        }
        match segments.into_iter().collect::<Option<Vec<_>>>() {
            Some(package) if self.nodes.contains(&package) => Resolution::Namespace,
            _ => Resolution::Unknown,
        }
    }

    fn resolve_operation(&self, written: &[&str], spelling: Spelling) -> Option<Resolution<'_>> {
        let (name, package) = written.split_last()?;
        let package = package
            .iter()
            .map(|segment| spelling.unspell(SpellKind::PathSegment, segment))
            .collect::<Option<Vec<_>>>()?;
        let package = PackagePath::try_from(package).ok()?;
        let name = spelling.unspell(SpellKind::Member, name)?;
        let (operation, offered) = self
            .operations
            .get_key_value(&QualifiedOperation { package, name })?;
        Some(Resolution::Operation { operation, offered })
    }

    /// The offered operation `written` names in another dialect's spelling
    /// of its last segment, and how `spelling` writes it: TypeScript's
    /// `inbox.send_item` is `inbox.sendItem`, Python's `inbox.sendItem` is
    /// `inbox.send_item`. `None` when `written` names no offered operation
    /// so, or names one as `spelling` writes it.
    #[must_use]
    pub fn misspelled(&self, written: &[&str], spelling: Spelling) -> Option<Misspelled> {
        if matches!(
            self.resolve_operation(written, spelling),
            Some(Resolution::Operation { .. })
        ) {
            return None;
        }
        let (name, package) = written.split_last()?;
        let package = package
            .iter()
            .map(|segment| Segment::new(*segment).ok())
            .collect::<Option<Vec<_>>>()?;
        let package = PackagePath::try_from(package).ok()?;
        [Spelling::TypeScript, Spelling::Python]
            .into_iter()
            .filter_map(|other| other.unspell(SpellKind::Member, name))
            .map(|name| QualifiedOperation {
                package: package.clone(),
                name,
            })
            .find(|operation| self.operations.contains_key(operation))
            .map(|operation| Misspelled {
                repair: spell_operation(spelling, &operation),
                operation,
            })
    }

    /// Whether `root` is the first segment of a package path.
    #[must_use]
    pub fn is_root(&self, root: &str) -> bool {
        Segment::new(root).is_ok_and(|root| self.nodes.contains(std::slice::from_ref(&root)))
    }
}

/// The code packages the tree is composed from, as member dispatch reads
/// them: a front end selects a value's member among these.
impl ExportView for Namespaces {
    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        self.code.get(path)
    }

    fn paths(&self) -> Vec<PackagePath> {
        self.code.keys().cloned().collect()
    }
}
