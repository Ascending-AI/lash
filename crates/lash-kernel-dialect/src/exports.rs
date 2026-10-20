//! What a package exports (FIG-6917): one entry per canonical segment.
//!
//! An export's kind is its variant, never the list it sits in. Packages
//! themselves (their code, digest and sets) are `lash-kernel-package`'s;
//! these shapes live here so that a front end's namespace resolution and
//! member dispatch can read them without linking packages.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{FunctionName, OperationRequirement, Signature, Type};
use serde::{Deserialize, Serialize};

use crate::PackagePath;
use crate::names::Segment;

/// One export of a package, under one canonical segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Export {
    /// A host effect the package declares; a runner serves it.
    Operation(OperationExport),
    /// A function a program calls by the package path.
    Function(Member),
    /// A branded value type and its members.
    Type(ValueType),
    /// An error. Its kernel error kind is `<package path>.<segment>`,
    /// derived and never written.
    Error,
}

/// The package's half of an operation: what it means, never how it runs.
/// Execution policy is a fact of the runner's link.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationExport {
    pub requirement: OperationRequirement,
    /// The intents the operation declares, compared with its runner's.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub intents: BTreeSet<Intent>,
    pub visibility: Visibility,
}

/// A declared intent of an operation, by its wire name (`start_process`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Intent(pub String);

/// Who may perform an operation.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Visibility {
    /// Any document, and the model.
    #[default]
    Public,
    /// Only the declaring package's own bodies.
    Internal,
}

/// A branded value type.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueType {
    /// The record a value of the type is: its declared fields, the brand
    /// `__lash_brand: Tuple(Enum(path), Enum(type))` and the building
    /// artifact `__lash_built: Bytes`.
    pub representation: Type,
    /// Statics, methods and properties share one map, so two of them never
    /// share a segment.
    pub members: BTreeMap<Segment, TypeMember>,
}

/// A member of a value type; its kind is its variant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TypeMember {
    /// Called on the type: no receiver.
    Static(Member),
    /// Called on a value: the receiver is the first slot.
    Method(Member),
    /// Read on a value: a getter whose one slot is the receiver.
    Property(Member),
}

impl TypeMember {
    pub fn member(&self) -> &Member {
        match self {
            Self::Static(member) | Self::Method(member) | Self::Property(member) => member,
        }
    }
}

/// A callable export: the definition that implements it and its canonical
/// slots, which are exactly that definition's signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub definition: FunctionName,
    /// Every slot fixed: an optional slot's type includes `Absent`, a rest
    /// slot is one `List`, options one record. A package value type's slot
    /// is `Record{__lash_brand: Tuple(Enum(path), Enum(type)), ..Any}`.
    pub signature: Signature,
    /// The member may wait: TypeScript awaits it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub asynchronous: bool,
    /// The last slot is a rest slot: a call gathers its trailing arguments
    /// into it. The receiver is never one.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub rest: bool,
}

/// The kernel type of a slot holding a value of the package type `ty` of
/// `package`: a record carrying exactly that brand, any other field open.
pub fn brand_type(package: &str, ty: &str) -> Type {
    Type::Record(lash_kernel_doc::RecordType {
        fields: vec![lash_kernel_doc::RecordTypeField {
            name: BRAND_FIELD.to_owned(),
            ty: brand_tuple(package, ty),
            optional: false,
        }],
        rest: Some(Box::new(Type::Any)),
    })
}

/// The narrowed type of a value type's brand field.
pub fn brand_tuple(package: &str, ty: &str) -> Type {
    Type::Tuple(vec![
        Type::Enum(vec![package.to_owned()]),
        Type::Enum(vec![ty.to_owned()]),
    ])
}

/// The field every package value holds its brand in: `(path, type)`.
pub const BRAND_FIELD: &str = "__lash_brand";

/// The field every package value holds its building artifact's digest in.
pub const BUILT_FIELD: &str = "__lash_built";

/// The visibility declared by the operation's owning package. An operation
/// outside the view belongs to an operation-only provider and is public.
pub fn operation_visibility(
    view: &dyn ExportView,
    operation: &crate::QualifiedOperation,
) -> Visibility {
    let mut path = operation.package.as_slice().to_vec();
    path.push(operation.name.clone());
    if classify_package_path(view, &path) == PackagePathClass::Internal {
        Visibility::Internal
    } else {
        Visibility::Public
    }
}

/// A path's authority under one pinned package surface.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackagePathClass {
    Public,
    Internal,
    Unknown,
    Outside,
}

/// Classify canonical segments without confusing a root with a textual prefix.
/// Exported functions, types and members are public; absent descendants stay
/// inside their installed root and cannot be supplied by a deferred provider.
pub fn classify_package_path(view: &dyn ExportView, path: &[Segment]) -> PackagePathClass {
    let Some(root) = path.first() else {
        return PackagePathClass::Outside;
    };
    if !view.namespace_roots().contains(root.as_str()) {
        return PackagePathClass::Outside;
    }
    for package in view.paths() {
        let Some(tail) = path.strip_prefix(package.as_slice()) else {
            continue;
        };
        let Some((name, members)) = tail.split_first() else {
            continue;
        };
        let Some(export) = view.exports(&package).and_then(|exports| exports.get(name)) else {
            continue;
        };
        return match (export, members) {
            (Export::Operation(operation), []) if operation.visibility == Visibility::Internal => {
                PackagePathClass::Internal
            }
            (Export::Operation(_) | Export::Function(_) | Export::Type(_) | Export::Error, []) => {
                PackagePathClass::Public
            }
            (Export::Type(ty), [member]) if ty.members.contains_key(member) => {
                PackagePathClass::Public
            }
            _ => PackagePathClass::Unknown,
        };
    }
    PackagePathClass::Unknown
}

/// The code exports of the packages a namespace is composed from.
pub trait ExportView {
    /// Whether `path` starts in an installed package namespace. Nested package
    /// paths reserve their common root too: an unknown child stays a package error.
    fn owns_namespace(&self, path: &str) -> bool {
        let root = path.split('.').next().unwrap_or(path);
        self.namespace_roots().contains(root)
    }

    /// Installed namespace roots, including libraries with no user exports.
    fn namespace_roots(&self) -> BTreeSet<String> {
        self.paths()
            .iter()
            .map(|package| package.as_slice()[0].as_str().to_owned())
            .collect()
    }

    /// The exports of the package that owns `path`, or `None` when no
    /// package of the view owns it.
    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>>;
    /// Every package path the view holds, in order.
    fn paths(&self) -> Vec<PackagePath>;
    /// What reserves `root`, a root no package path may take: lash's
    /// definition roots, each installed dialect's globals and the names its
    /// printer generates. `None` for a root a package may take.
    fn reserves(&self, root: &str) -> Option<String> {
        let _ = root;
        None
    }
}

/// A view that holds no package.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoPackages;

impl ExportView for NoPackages {
    fn exports(&self, _: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        None
    }

    fn paths(&self) -> Vec<PackagePath> {
        Vec::new()
    }
}

/// `view` with the roots `reserves` reserves beside its own: what an
/// embedding with installed dialects composes namespaces under.
pub struct Reserving<'a> {
    pub view: &'a dyn ExportView,
    pub reserves: &'a dyn Fn(&str) -> Option<String>,
}

impl ExportView for Reserving<'_> {
    fn namespace_roots(&self) -> BTreeSet<String> {
        self.view.namespace_roots()
    }

    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, Export>> {
        self.view.exports(path)
    }

    fn paths(&self) -> Vec<PackagePath> {
        self.view.paths()
    }

    fn reserves(&self, root: &str) -> Option<String> {
        self.view.reserves(root).or_else(|| (self.reserves)(root))
    }
}
