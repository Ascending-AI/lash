//! Export signatures and branded representations share the operation type DAG.
use std::collections::BTreeMap;

use lash_kernel_dialect::{FunctionMember, NamespaceKind, NamespaceMember, TypeMember, ValueType};
use lash_kernel_value::{Convention, QualifiedName, Signature, Visibility, names::Segment};
use serde::{Deserialize, Serialize};

use super::{Parameter, TypeTable, types::Types};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Export {
    visibility: Visibility,
    kind: Kind,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Kind {
    Function(Function),
    Type {
        representation: usize,
        members: BTreeMap<Segment, Member>,
    },
    Error,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Member {
    Static(Function),
    Method(Function),
    Property(Function),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Function {
    name: QualifiedName,
    convention: Convention,
    params: Vec<Parameter>,
    result: usize,
    asynchronous: bool,
    rest: bool,
}

impl Function {
    fn new<'a>(function: &'a FunctionMember, types: &mut TypeTable<'a>) -> Self {
        Self {
            name: function.name.clone(),
            convention: function.convention,
            params: function
                .signature
                .params
                .iter()
                .map(|p| Parameter {
                    name: p.name.clone(),
                    ty: types.intern(&p.ty),
                    optional: p.optional,
                })
                .collect(),
            result: types.intern(&function.signature.result),
            asynchronous: function.asynchronous,
            rest: function.rest,
        }
    }

    fn indices(&self, indices: &mut Vec<usize>) {
        indices.extend(self.params.iter().map(|p| p.ty));
        indices.push(self.result);
    }

    fn expand(self, types: &Types) -> FunctionMember {
        FunctionMember {
            name: self.name,
            convention: self.convention,
            signature: Signature {
                params: self
                    .params
                    .into_iter()
                    .map(|p| lash_kernel_value::Param {
                        name: p.name,
                        ty: types.get(p.ty),
                        optional: p.optional,
                    })
                    .collect(),
                result: types.get(self.result),
            },
            asynchronous: self.asynchronous,
            rest: self.rest,
        }
    }
}

impl Export {
    pub(super) fn new<'a>(member: &'a NamespaceMember, types: &mut TypeTable<'a>) -> Option<Self> {
        let kind = match &member.kind {
            NamespaceKind::Operation(_) => return None,
            NamespaceKind::Function(function) => Kind::Function(Function::new(function, types)),
            NamespaceKind::Type(ty) => Kind::Type {
                representation: types.intern(&ty.representation),
                members: ty
                    .members
                    .iter()
                    .map(|(name, member)| {
                        let function = Function::new(member.member(), types);
                        let member = match member {
                            TypeMember::Static(_) => Member::Static(function),
                            TypeMember::Method(_) => Member::Method(function),
                            TypeMember::Property(_) => Member::Property(function),
                        };
                        (name.clone(), member)
                    })
                    .collect(),
            },
            NamespaceKind::Error => Kind::Error,
        };
        Some(Self {
            visibility: member.visibility,
            kind,
        })
    }

    pub(super) fn indices(&self) -> Vec<usize> {
        let mut indices = Vec::new();
        match &self.kind {
            Kind::Function(function) => function.indices(&mut indices),
            Kind::Type {
                representation,
                members,
            } => {
                indices.push(*representation);
                for member in members.values() {
                    match member {
                        Member::Static(f) | Member::Method(f) | Member::Property(f) => {
                            f.indices(&mut indices)
                        }
                    }
                }
            }
            Kind::Error => {}
        }
        indices
    }

    pub(super) fn expand(self, types: &Types) -> NamespaceMember {
        NamespaceMember {
            visibility: self.visibility,
            kind: match self.kind {
                Kind::Function(function) => NamespaceKind::Function(function.expand(types)),
                Kind::Type {
                    representation,
                    members,
                } => NamespaceKind::Type(ValueType {
                    representation: types.get(representation),
                    members: members
                        .into_iter()
                        .map(|(name, member)| {
                            (
                                name,
                                match member {
                                    Member::Static(f) => TypeMember::Static(f.expand(types)),
                                    Member::Method(f) => TypeMember::Method(f.expand(types)),
                                    Member::Property(f) => TypeMember::Property(f.expand(types)),
                                },
                            )
                        })
                        .collect(),
                }),
                Kind::Error => NamespaceKind::Error,
            },
        }
    }
}
