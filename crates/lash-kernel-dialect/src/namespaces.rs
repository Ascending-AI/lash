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

use lash_kernel_doc::{EffectName, OperationRequirement};
use serde::{Deserialize, Serialize};

use crate::{
    EffectControl, Export, ExportView, PackagePath, QualifiedOperation, Segment, SpellKind,
    Spelling, TypeMember,
};

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
    pub source: OfferedSource,
}

impl Offered {
    /// An operation the catalog tool `tool` serves under `signature`: no
    /// controls, not a read, its contract the signature's.
    #[must_use]
    pub fn tool(tool: impl Into<String>, signature: lash_kernel_doc::Signature) -> Self {
        Self {
            requirement: OperationRequirement::of_signature(signature),
            controls: BTreeSet::new(),
            read: false,
            source: OfferedSource::Catalog { tool: tool.into() },
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

/// A [`QualifiedOperation`] serialized as its effect name
/// (`inbox.send_item`), for a field: `#[serde(with = "operation_text")]`.
pub mod operation_text {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::QualifiedOperation;

    /// # Errors
    ///
    /// The serializer's.
    pub fn serialize<S: Serializer>(
        operation: &QualifiedOperation,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(operation.effect_name().as_str())
    }

    /// # Errors
    ///
    /// A text that is no canonical operation.
    pub fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<QualifiedOperation, D::Error> {
        parse(&String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }

    /// The operation `text` names.
    ///
    /// # Errors
    ///
    /// Why `text` is no canonical operation.
    pub fn parse(text: &str) -> Result<QualifiedOperation, String> {
        let effect = lash_kernel_doc::EffectName::new(text).map_err(|error| error.to_string())?;
        QualifiedOperation::from_effect(&effect).map_err(|error| error.to_string())
    }
}

/// A map keyed by [`QualifiedOperation`], serialized with each key as its
/// effect name, for a field: `#[serde(with = "operation_keys")]`.
pub mod operation_keys {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    use crate::QualifiedOperation;

    /// # Errors
    ///
    /// The serializer's.
    pub fn serialize<V: Serialize, S: Serializer>(
        map: &BTreeMap<QualifiedOperation, V>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.collect_map(
            map.iter()
                .map(|(operation, value)| (operation.effect_name().to_string(), value)),
        )
    }

    /// # Errors
    ///
    /// A key that is no canonical operation.
    pub fn deserialize<'de, V: Deserialize<'de>, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<BTreeMap<QualifiedOperation, V>, D::Error> {
        BTreeMap::<String, V>::deserialize(deserializer)?
            .into_iter()
            .map(|(key, value)| {
                super::operation_text::parse(&key)
                    .map(|operation| (operation, value))
                    .map_err(serde::de::Error::custom)
            })
            .collect()
    }
}

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
    /// An offered effect name that is no canonical operation.
    #[error("`{effect}` is not an operation: {message}")]
    NotAnOperation { effect: String, message: String },
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
    /// [`NamespaceRefusal`]: a package path equal to another package's
    /// export, or a root that is reserved (`lash`, a dialect global, a
    /// printer-generated name, a root of lash's own packages).
    pub fn build(view: &dyn ExportView, operations: Operations) -> Result<Self, NamespaceRefusal> {
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
            let node = exports
                .entry(operation.package.as_slice().to_vec())
                .or_default();
            let named = operation.effect_name().to_string();
            if let Some(other) = node.insert(operation.name.clone(), named.clone())
                && other != named
            {
                return Err(NamespaceRefusal::PathIsExport {
                    path: named,
                    package: dotted(operation.package.iter()),
                    export: other,
                });
            }
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

    /// The tree of the operations `signatures` names, each served by the
    /// catalog tool of its effect name, and those of `controls` declaring
    /// theirs: what an embedder that types its effects itself, with no tool
    /// catalog, lowers against.
    ///
    /// # Errors
    ///
    /// [`NamespaceRefusal::NotAnOperation`] for an effect name that is no
    /// canonical operation; otherwise as [`Self::build`].
    pub fn of_signatures(
        signatures: &BTreeMap<EffectName, lash_kernel_doc::Signature>,
        controls: &BTreeMap<EffectName, BTreeSet<EffectControl>>,
    ) -> Result<Self, NamespaceRefusal> {
        let mut operations = Operations::new();
        for (effect, signature) in signatures {
            let operation = QualifiedOperation::from_effect(effect).map_err(|error| {
                NamespaceRefusal::NotAnOperation {
                    effect: effect.to_string(),
                    message: error.to_string(),
                }
            })?;
            let offered = Offered::tool(effect.as_str(), signature.clone())
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
    pub fn signatures(&self) -> BTreeMap<EffectName, lash_kernel_doc::Signature> {
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
