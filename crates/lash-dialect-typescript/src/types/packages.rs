//! What the type analysis knows of the packages in view: what each
//! package function and static gives, the value type each annotation name
//! stands for, and what each member of a value type gives. A package
//! member's declared result is trusted as a tool's is (`TS_TYPED_*`): its
//! definition's signature is its contract.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_dialect::dispatch::Brand;
use lash_kernel_dialect::{
    FunctionMember, NamespaceKind, NamespaceView, SpellKind, Spelling, TypeMember,
};

use super::Ty;

/// What a member of a value type is, to a reader of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MemberKind {
    Static,
    Method,
    Property,
}

/// What a call of a member gives: its declared result, and whether it is
/// asynchronous, so that only an awaited call gives that result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Gives {
    pub(crate) result: Ty,
    pub(crate) asynchronous: bool,
}

impl Gives {
    pub(crate) fn of(member: &FunctionMember) -> Self {
        Self {
            result: Ty::decoded(&member.signature.result),
            asynchronous: member.asynchronous,
        }
    }

    /// What a call gives where it stands: the result of a synchronous
    /// member, or of any member once awaited; else a promise.
    pub(crate) fn called(&self, awaited: bool) -> Ty {
        if awaited || !self.asynchronous {
            self.result.clone()
        } else {
            Ty::Unknown
        }
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct Packages {
    /// Every package path's first segment.
    roots: BTreeSet<String>,
    /// Each package function and static, by the dotted path the source
    /// spells it with: `dataframe.col`, `dataframe.Context.create`.
    calls: BTreeMap<String, Gives>,
    /// Each value type, by the dotted name an annotation spells it with:
    /// `dataframe.DataQuanta`.
    types: BTreeMap<String, Ty>,
    /// Each method and property of a value type, by its brand and the
    /// name the source spells it with.
    members: BTreeMap<(Brand, String), (MemberKind, Gives)>,
}

impl Packages {
    pub(crate) fn of(view: &dyn NamespaceView) -> Self {
        let spelling = Spelling::TypeScript;
        let mut packages = Self::default();
        for package in view.paths() {
            let Some(exports) = view.exports(&package) else {
                continue;
            };
            let path = package
                .iter()
                .map(|segment| spelling.spell(SpellKind::PathSegment, segment))
                .collect::<Vec<_>>()
                .join(".");
            if let Some(root) = package.iter().next() {
                packages.roots.insert(root.as_str().to_owned());
            }
            for (name, export) in exports {
                match &export.kind {
                    NamespaceKind::Function(member) => {
                        let spelled = spelling.spell(SpellKind::Member, name);
                        packages
                            .calls
                            .insert(format!("{path}.{spelled}"), Gives::of(member));
                    }
                    NamespaceKind::Type(ty) => {
                        let spelled =
                            format!("{path}.{}", spelling.spell(SpellKind::TypeName, name));
                        let brand = Brand {
                            package: package.clone(),
                            ty: name.clone(),
                        };
                        for (member_name, member) in &ty.members {
                            let member_spelled = spelling.spell(SpellKind::Member, member_name);
                            let (kind, member) = match member {
                                TypeMember::Static(member) => {
                                    packages.calls.insert(
                                        format!("{spelled}.{member_spelled}"),
                                        Gives::of(member),
                                    );
                                    (MemberKind::Static, member)
                                }
                                TypeMember::Method(member) => (MemberKind::Method, member),
                                TypeMember::Property(member) => (MemberKind::Property, member),
                            };
                            packages
                                .members
                                .insert((brand.clone(), member_spelled), (kind, Gives::of(member)));
                        }
                        packages.types.insert(spelled, Ty::Package(Box::new(brand)));
                    }
                    _ => {}
                }
            }
        }
        packages
    }

    /// Whether `name` is the first segment of a package path.
    pub(crate) fn is_root(&self, name: &str) -> bool {
        self.roots.contains(name)
    }

    /// What a package function or static spelled `path` gives.
    pub(crate) fn call(&self, path: &str) -> Option<&Gives> {
        self.calls.get(path)
    }

    /// What the member `name` of values of `brand` is and gives.
    pub(crate) fn member(&self, brand: &Brand, name: &str) -> Option<&(MemberKind, Gives)> {
        self.members.get(&(brand.clone(), name.to_owned()))
    }

    /// Each value type by its annotation name, for the type analysis's
    /// aliases.
    pub(crate) fn types(&self) -> impl Iterator<Item = (&String, &Ty)> {
        self.types.iter()
    }
}
