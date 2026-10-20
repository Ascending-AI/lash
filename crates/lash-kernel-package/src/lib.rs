//! Packages (FIG-6917): what a host and lash install through one path.
//!
//! A package is a dotted namespace path. It exports operations (host
//! effects a runner serves) and code (content-addressed kernel functions
//! and branded value types), one export per canonical segment. Each build
//! of one is an artifact keyed by `(path, digest)`: unchanged source over a
//! changed dependency is a new artifact beside the old one. A package set
//! holds one artifact per line and is dependency-closed. A node holds its
//! artifacts once ([`HeldPackages`]); the parent's admission and every
//! worker's registry are derived from that one registration, and every
//! reader of a cell's packages takes a [`PackageView`] of one set.

mod build;
mod frame;
mod held;
mod install;
mod lash;
mod package;
mod refusal;
mod set;
mod shape;
pub mod shipped_display;
mod stale;
mod surface;
pub use lash_helper_labels::{Comparison, LabelError, MemberDisplay};
#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
mod laws;

pub use build::PackageSources;
pub use held::{HeldPackages, PackageView, SetNotHeld};
pub use install::PackageInstall;
pub use lash_kernel_dialect::{
    Export, Intent, Member, MemberPath, OperationExport, PackagePath, QualifiedOperation, Segment,
    TypeMember, ValueType, Visibility,
};
pub use lash_kernel_doc::{
    ArtifactDigest, ContractId, InvalidDigest, OperationRequirement, PackageSetDigest,
};
pub use package::{ArtifactKey, Code, Origin, Package, PackageDisplay, PackageHeader, Root};
pub use refusal::{Declared, NameRole, PackageRefusal, SetHolds};
pub use set::PackageSet;
pub use shape::STALE_VALUE;
pub use stale::StaleValue;

use lash_kernel_doc::FunctionName;

/// Every member a package declares, with the definition that implements
/// it.
pub(crate) fn members(package: &Package) -> Vec<(MemberPath, &FunctionName)> {
    let mut members = Vec::new();
    for (segment, export) in package.exports() {
        match export {
            Export::Function(member) => members.push((
                MemberPath {
                    export: segment.clone(),
                    member: None,
                },
                &member.definition,
            )),
            Export::Type(ty) => {
                for (name, member) in &ty.members {
                    members.push((
                        MemberPath {
                            export: segment.clone(),
                            member: Some(name.clone()),
                        },
                        &member.member().definition,
                    ));
                }
            }
            Export::Operation(_) | Export::Error => {}
        }
    }
    members
}
