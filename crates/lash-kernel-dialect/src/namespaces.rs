//! One runtime projection of the accepted package advertisement. A missing
//! member of an enumerated package never falls through to a deferred provider.
use crate::{NamespaceKind, NamespaceMember, NamespaceView, SpellKind, Spelling};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use {
    lash_kernel_value::names::PackagePath, lash_kernel_value::names::QualifiedOperation,
    lash_kernel_value::names::Segment, lash_kernel_value::package::Visibility,
};
pub use {lash_kernel_value::operation_keys, lash_kernel_value::operation_text};

pub fn spell_operation(spelling: Spelling, operation: &QualifiedOperation) -> String {
    operation
        .package
        .iter()
        .map(|segment| spelling.spell(SpellKind::PathSegment, segment))
        .chain(std::iter::once(
            spelling.spell(SpellKind::Member, &operation.name),
        ))
        .collect::<Vec<_>>()
        .join(".")
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum NamespaceRefusal {
    #[error("conflicting source requirements {first:?} and {second:?}")]
    ImportNameConflict {
        first: Box<lash_kernel_value::Import>,
        second: Box<lash_kernel_value::Import>,
    },
    #[error("the package export `{name:?}` in Link {link} is not loaded")]
    MissingExport {
        link: lash_kernel_value::LinkId,
        name: lash_kernel_value::ExportName,
    },
    #[error("the package path `{path}` conflicts with `{other}`")]
    PathConflict { path: String, other: String },
    #[error("the package path `{path}` takes reserved root `{root}`")]
    ReservedRoot { path: String, root: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolution<'a> {
    Operation {
        operation: QualifiedOperation,
        offered: &'a crate::OperationSignature,
    },
    Export {
        package: &'a PackagePath,
        segment: &'a Segment,
        export: &'a NamespaceKind,
        member: Option<(&'a Segment, &'a crate::TypeMember)>,
    },
    Deferred {
        operation: QualifiedOperation,
    },
    Namespace,
    Unknown,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Misspelled {
    pub operation: QualifiedOperation,
    pub repair: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Namespaces {
    packages: BTreeMap<PackagePath, BTreeMap<Segment, NamespaceMember>>,
    deferred: BTreeSet<PackagePath>,
}
impl Namespaces {
    /// Project the accepted package members from their linked declarations.
    /// The resolver reads held content only; it performs no publication or fulfilment.
    pub fn project<'a>(
        packages: impl IntoIterator<Item = &'a lash_kernel_value::Package>,
        deferred: BTreeSet<PackagePath>,
        export: &dyn Fn(
            lash_kernel_value::LinkId,
            &lash_kernel_value::ExportName,
        ) -> Option<lash_kernel_doc::Export>,
        reserves: &dyn Fn(&str) -> bool,
    ) -> Result<Self, NamespaceRefusal> {
        use {lash_kernel_value::MemberKind, lash_kernel_value::QualifiedName};
        fn function(
            name: QualifiedName,
            f: lash_kernel_doc::FunctionExport,
        ) -> crate::FunctionMember {
            crate::FunctionMember {
                name,
                signature: f.signature,
                convention: f.convention.semantic(),
                asynchronous: f.asynchronous,
                rest: f.rest,
            }
        }
        let mut projected = BTreeMap::new();
        for package in packages {
            let mut members = BTreeMap::new();
            for (segment, member) in &package.members {
                let name = QualifiedName {
                    package: package.path.clone(),
                    member: segment.clone(),
                    type_member: None,
                };
                let kind = match &member.kind {
                    MemberKind::Operation(offer) => NamespaceKind::Operation(offer.clone().into()),
                    MemberKind::Export {
                        link,
                        export: exported,
                    } => {
                        match export(*link, exported).ok_or_else(|| {
                            NamespaceRefusal::MissingExport {
                                link: *link,
                                name: exported.clone(),
                            }
                        })? {
                            lash_kernel_doc::Export::Function(f) => {
                                NamespaceKind::Function(function(name, f))
                            }
                            lash_kernel_doc::Export::Error { .. } => NamespaceKind::Error,
                            lash_kernel_doc::Export::Type(ty) => {
                                NamespaceKind::Type(crate::ValueType {
                                    representation: ty.representation,
                                    members: ty
                                        .members
                                        .into_iter()
                                        .map(|(segment, member)| {
                                            let mut name = name.clone();
                                            name.type_member = Some(segment.clone());
                                            let member = match member {
                                                lash_kernel_doc::TypeMember::Static(f) => {
                                                    crate::TypeMember::Static(function(name, f))
                                                }
                                                lash_kernel_doc::TypeMember::Method(f) => {
                                                    crate::TypeMember::Method(function(name, f))
                                                }
                                                lash_kernel_doc::TypeMember::Property(f) => {
                                                    crate::TypeMember::Property(function(name, f))
                                                }
                                            };
                                            (segment, member)
                                        })
                                        .collect(),
                                })
                            }
                        }
                    }
                };
                members.insert(
                    segment.clone(),
                    NamespaceMember {
                        visibility: member.visibility,
                        kind,
                    },
                );
            }
            if projected.insert(package.path.clone(), members).is_some() {
                let path = dotted(&package.path);
                return Err(NamespaceRefusal::PathConflict {
                    path: path.clone(),
                    other: path,
                });
            }
        }
        Self::new(projected, deferred, reserves)
    }

    /// The runtime supplies one projection of each accepted package. All kinds
    /// share the same member map. Deferred paths contain no enumerated members.
    pub fn new(
        packages: BTreeMap<PackagePath, BTreeMap<Segment, NamespaceMember>>,
        deferred: BTreeSet<PackagePath>,
        reserves: &dyn Fn(&str) -> bool,
    ) -> Result<Self, NamespaceRefusal> {
        let paths: Vec<_> = packages.keys().chain(deferred.iter()).collect();
        for (at, path) in paths.iter().enumerate() {
            let root = &path.as_slice()[0];
            if reserves(root.as_str()) {
                return Err(NamespaceRefusal::ReservedRoot {
                    path: dotted(path),
                    root: root.to_string(),
                });
            }
            for other in paths.iter().skip(at + 1) {
                let overlaps = path == other
                    || ((deferred.contains(*path) || deferred.contains(*other))
                        && (other.as_slice().starts_with(path.as_slice())
                            || path.as_slice().starts_with(other.as_slice())));
                if overlaps {
                    return Err(conflict(path, other));
                }
            }
            for (owner, members) in &packages {
                if let Some(tail) = path.as_slice().strip_prefix(owner.as_slice())
                    && let Some(first) = tail.first()
                    && members.contains_key(first)
                {
                    return Err(conflict(path, owner));
                }
            }
        }
        Ok(Self { packages, deferred })
    }

    pub fn packages(&self) -> &BTreeMap<PackagePath, BTreeMap<Segment, NamespaceMember>> {
        &self.packages
    }

    /// Deferred package paths have no enumerated members to project.
    pub fn deferred(&self) -> &BTreeSet<PackagePath> {
        &self.deferred
    }

    pub fn operations(
        &self,
    ) -> impl Iterator<Item = (QualifiedOperation, &crate::OperationSignature)> {
        self.packages.iter().flat_map(|(package, members)| {
            members
                .iter()
                .filter_map(move |(name, member)| match &member.kind {
                    NamespaceKind::Operation(offer) if member.visibility == Visibility::Public => {
                        Some((
                            QualifiedOperation {
                                package: package.clone(),
                                name: name.clone(),
                            },
                            offer,
                        ))
                    }
                    _ => None,
                })
        })
    }
    pub fn operation(
        &self,
        effect: &lash_kernel_value::EffectName,
    ) -> Option<(QualifiedOperation, &crate::OperationSignature)> {
        let operation = QualifiedOperation::from_effect(effect).ok()?;
        let member = self
            .packages
            .get(&operation.package)?
            .get(&operation.name)?;
        match &member.kind {
            NamespaceKind::Operation(offer) if member.visibility == Visibility::Public => {
                Some((operation, offer))
            }
            _ => None,
        }
    }
    pub fn controls(
        &self,
        effect: &lash_kernel_value::EffectName,
    ) -> Option<BTreeSet<crate::EffectControl>> {
        let (_, offer) = self.operation(effect)?;
        let controls = controls(&offer.controls);
        (!controls.is_empty()).then_some(controls)
    }
    pub fn resolve(&self, written: &[&str], spelling: Spelling) -> Resolution<'_> {
        for (package, members) in &self.packages {
            if written.len() < package.len()
                || !package
                    .iter()
                    .zip(written)
                    .all(|(a, b)| spelling.spell(SpellKind::PathSegment, a) == *b)
            {
                continue;
            }
            let tail = &written[package.len()..];
            let Some((word, rest)) = tail.split_first() else {
                return Resolution::Namespace;
            };
            for (segment, member) in members {
                if member.visibility != Visibility::Public {
                    continue;
                }
                let kind = match &member.kind {
                    NamespaceKind::Type(_) | NamespaceKind::Error => SpellKind::TypeName,
                    _ => SpellKind::Member,
                };
                if spelling.spell(kind, segment) != *word {
                    continue;
                }
                if let NamespaceKind::Operation(offered) = &member.kind {
                    return if rest.is_empty() {
                        Resolution::Operation {
                            operation: QualifiedOperation {
                                package: package.clone(),
                                name: segment.clone(),
                            },
                            offered,
                        }
                    } else {
                        Resolution::Unknown
                    };
                }
                let nested = match (&member.kind, rest) {
                    (_, []) => None,
                    (NamespaceKind::Type(ty), [word]) => match ty
                        .members
                        .iter()
                        .find(|(name, _)| spelling.spell(SpellKind::Member, name) == *word)
                    {
                        Some(member) => Some(member),
                        None => return Resolution::Unknown,
                    },
                    _ => return Resolution::Unknown,
                };
                return Resolution::Export {
                    package,
                    segment,
                    export: &member.kind,
                    member: nested,
                };
            }
            return Resolution::Unknown;
        }
        for package in &self.deferred {
            if written.len() == package.len() + 1
                && package
                    .iter()
                    .zip(written)
                    .all(|(a, b)| spelling.spell(SpellKind::PathSegment, a) == *b)
                && let Some(name) = spelling.unspell(SpellKind::Member, written[package.len()])
            {
                return Resolution::Deferred {
                    operation: QualifiedOperation {
                        package: package.clone(),
                        name,
                    },
                };
            }
        }
        if self
            .packages
            .keys()
            .chain(self.deferred.iter())
            .any(|package| {
                written.len() <= package.len()
                    && package
                        .iter()
                        .zip(written)
                        .all(|(a, b)| spelling.spell(SpellKind::PathSegment, a) == *b)
            })
        {
            Resolution::Namespace
        } else {
            Resolution::Unknown
        }
    }
    pub fn misspelled(&self, written: &[&str], spelling: Spelling) -> Option<Misspelled> {
        if matches!(
            self.resolve(written, spelling),
            Resolution::Operation { .. } | Resolution::Deferred { .. }
        ) {
            return None;
        }
        [Spelling::TypeScript, Spelling::Python]
            .into_iter()
            .find_map(|other| match self.resolve(written, other) {
                Resolution::Operation { operation, .. } | Resolution::Deferred { operation } => {
                    Some(Misspelled {
                        repair: spell_operation(spelling, &operation),
                        operation,
                    })
                }
                _ => None,
            })
    }
    /// Collisions in a discovered surface, spelled as the dialect sees it.
    /// No binding is renamed and no namespace changes lexical resolution.
    pub fn collisions(
        &self,
        bindings: &BTreeSet<lash_kernel_value::Name>,
        spelling: Spelling,
    ) -> Vec<lash_kernel_value::NamespaceCollision> {
        self.packages
            .keys()
            .chain(self.deferred.iter())
            .filter_map(|path| {
                let root = spelling.spell(SpellKind::PathSegment, &path.as_slice()[0]);
                bindings
                    .contains(&lash_kernel_value::Name::new(&root))
                    .then(|| lash_kernel_value::NamespaceCollision {
                        namespace: dotted(path),
                        binding_name: root,
                    })
            })
            .collect()
    }

    /// Facts for a call whose root the caller has resolved to a lexical local.
    /// A deferred grant names an operation just as an enumerated offer does.
    pub fn shadowed_call(
        &self,
        path: &[&str],
        spelling: Spelling,
    ) -> Option<lash_kernel_value::NamespaceShadowing> {
        let operation = match self.resolve(path, spelling) {
            Resolution::Operation { operation, .. } | Resolution::Deferred { operation } => {
                operation
            }
            _ => return None,
        };
        Some(lash_kernel_value::NamespaceShadowing {
            binding_name: path.first()?.to_string(),
            binding_origin: None,
            namespace: dotted(&operation.package),
            call_path: path.join("."),
        })
    }

    pub fn is_root(&self, root: &str) -> bool {
        self.packages
            .keys()
            .chain(self.deferred.iter())
            .any(|path| path.as_slice()[0].as_str() == root)
    }
}
impl NamespaceView for Namespaces {
    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, NamespaceMember>> {
        self.packages.get(path)
    }
    fn paths(&self) -> Vec<PackagePath> {
        self.packages.keys().cloned().collect()
    }
}
fn dotted(path: &PackagePath) -> String {
    path.iter()
        .map(Segment::as_str)
        .collect::<Vec<_>>()
        .join(".")
}
fn conflict(path: &PackagePath, other: &PackagePath) -> NamespaceRefusal {
    NamespaceRefusal::PathConflict {
        path: dotted(path),
        other: dotted(other),
    }
}
pub fn controls(value: &lash_kernel_value::TurnControls) -> BTreeSet<crate::EffectControl> {
    let mut controls = BTreeSet::new();
    if value.contains(lash_kernel_value::TurnControlKind::Finish) {
        controls.insert(crate::EffectControl::Finish);
    }
    if value.contains(lash_kernel_value::TurnControlKind::SwitchAgentFrame) {
        controls.insert(crate::EffectControl::SwitchAgentFrame);
    }
    controls
}

/// Deferred calls pack their arguments into one JSON list. Resolution narrows
/// that payload at first use; no provider is invoked while lowering.
pub fn deferred_signature() -> lash_kernel_value::types::Signature {
    use {
        lash_kernel_value::names::Name, lash_kernel_value::types::Param,
        lash_kernel_value::types::Signature, lash_kernel_value::types::Type,
    };
    Signature {
        params: vec![Param {
            name: Name::new("args"),
            ty: Type::Json,
            optional: false,
        }],
        result: Type::Json,
    }
}
pub fn service_name(dialect: &str, member: &str) -> lash_kernel_value::names::QualifiedName {
    lash_kernel_value::names::QualifiedName {
        package: PackagePath::new(
            Segment::new(dialect).unwrap_or_else(|error| panic!("dialect package: {error}")),
        ),
        member: Segment::new(member).unwrap_or_else(|error| panic!("service member: {error}")),
        type_member: None,
    }
}
pub fn sleep_signature() -> lash_kernel_value::types::Signature {
    use {
        lash_kernel_value::names::Name, lash_kernel_value::types::Param,
        lash_kernel_value::types::Signature, lash_kernel_value::types::Type,
    };
    Signature {
        params: vec![Param {
            name: Name::new("milliseconds"),
            ty: Type::Int,
            optional: false,
        }],
        result: Type::Null,
    }
}
