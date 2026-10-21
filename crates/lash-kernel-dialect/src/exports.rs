//! What a package exports (FIG-6917): one entry per canonical segment.
//!
//! An export's kind is its variant, never the list it sits in. Packages
//! themselves (their code, digest and sets) are `lash-kernel-package`'s;
//! these shapes live here so that a front end's namespace resolution and
//! member dispatch can read them without linking packages.

use std::collections::BTreeMap;

use lash_vocabulary::{Signature, Type};

use serde::{Deserialize, Serialize};

use lash_vocabulary::names::Segment;
use lash_vocabulary::{
    names::{Convention, PackagePath, QualifiedName},
    package::{OperationOffer, Visibility},
};

/// One export of a package, under one canonical segment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NamespaceKind {
    /// A host effect the package declares; a runner serves it.
    Operation(OperationSignature),
    /// A function a program calls by the package path.
    Function(FunctionMember),
    /// A branded value type and its members.
    Type(ValueType),
    /// An error. Its kernel error kind is `<package path>.<segment>`,
    /// derived and never written.
    Error,
}

/// Source lowering needs the signature and scheduling metadata, never a
/// contract chosen by fulfilment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationSignature {
    pub signature: Signature,
    pub controls: lash_vocabulary::TurnControls,
    pub mode: lash_vocabulary::OperationMode,
}
impl From<OperationOffer> for OperationSignature {
    fn from(offer: OperationOffer) -> Self {
        Self {
            signature: offer.requirement.signature,
            controls: offer.controls,
            mode: offer.mode,
        }
    }
}

/// One accepted visible member, projected by the runtime from a Package and
/// its linked Export. This is lowering metadata, not publication authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceMember {
    pub visibility: Visibility,
    pub kind: NamespaceKind,
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
    Static(FunctionMember),
    /// Called on a value: the receiver is the first slot.
    Method(FunctionMember),
    /// Read on a value: a getter whose one slot is the receiver.
    Property(FunctionMember),
}

impl TypeMember {
    pub fn member(&self) -> &FunctionMember {
        match self {
            Self::Static(member) | Self::Method(member) | Self::Property(member) => member,
        }
    }
}

/// A callable export: the definition that implements it and its canonical
/// slots, which are exactly that definition's signature.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionMember {
    pub name: QualifiedName,
    pub convention: Convention,
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
    Type::Record(lash_vocabulary::RecordType {
        fields: vec![lash_vocabulary::RecordTypeField {
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

/// The runtime projection consumed by member dispatch. Core and Standard use
/// neutral Package data and never receive this view.
pub trait NamespaceView {
    fn exports(&self, path: &PackagePath) -> Option<&BTreeMap<Segment, NamespaceMember>>;
    fn paths(&self) -> Vec<PackagePath>;
}
