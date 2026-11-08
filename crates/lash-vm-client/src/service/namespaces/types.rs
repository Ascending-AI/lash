//! A child-before-parent DAG of all nested signature types.
use std::collections::HashMap;

use lash_kernel_value::types::{
    MapType, Param, RecordRest, RecordType, RecordTypeField, Signature, Type,
};
use serde::{Deserialize, Serialize, de::Error};

use super::Parameter;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum TypeNode {
    Leaf(Type),
    Tuple(Vec<usize>),
    List(usize),
    Map {
        key: usize,
        value: usize,
    },
    Set(usize),
    Record {
        fields: Vec<Field>,
        rest: Rest,
    },
    Function {
        params: Vec<Parameter>,
        result: usize,
    },
    Task(usize),
    Union(Vec<usize>),
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Field {
    name: String,
    ty: usize,
    optional: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Rest {
    Closed,
    Drop,
    Keep(usize),
}

impl TypeNode {
    fn of<'a>(ty: &'a Type, mut intern: impl FnMut(&'a Type) -> usize) -> Self {
        match ty {
            Type::Tuple(items) => Self::Tuple(items.iter().map(intern).collect()),
            Type::List(ty) => Self::List(intern(ty)),
            Type::Map(map) => Self::Map {
                key: intern(&map.key),
                value: intern(&map.value),
            },
            Type::Set(ty) => Self::Set(intern(ty)),
            Type::Record(record) => Self::Record {
                fields: record
                    .fields
                    .iter()
                    .map(|field| Field {
                        name: field.name.clone(),
                        ty: intern(&field.ty),
                        optional: field.optional,
                    })
                    .collect(),
                rest: match &record.rest {
                    RecordRest::Closed => Rest::Closed,
                    RecordRest::Drop => Rest::Drop,
                    RecordRest::Keep(ty) => Rest::Keep(intern(ty)),
                },
            },
            Type::Function(signature) => Self::Function {
                params: signature
                    .params
                    .iter()
                    .map(|param| Parameter {
                        name: param.name.clone(),
                        ty: intern(&param.ty),
                        optional: param.optional,
                    })
                    .collect(),
                result: intern(&signature.result),
            },
            Type::Task(ty) => Self::Task(intern(ty)),
            Type::Union(items) => Self::Union(items.iter().map(intern).collect()),
            Type::Any
            | Type::Json
            | Type::Null
            | Type::Absent
            | Type::Bool
            | Type::Int
            | Type::Float
            | Type::Number
            | Type::Text
            | Type::Bytes
            | Type::Timestamp
            | Type::Enum(_)
            | Type::Error
            | Type::Handle(_) => Self::Leaf(ty.clone()),
        }
    }

    /// The type this node is, its children built by `resolve`.
    fn build(&self, mut resolve: impl FnMut(usize) -> Type) -> Type {
        match self {
            Self::Leaf(ty) => ty.clone(),
            Self::Tuple(items) => Type::Tuple(items.iter().map(|i| resolve(*i)).collect()),
            Self::List(ty) => Type::List(Box::new(resolve(*ty))),
            Self::Map { key, value } => Type::Map(Box::new(MapType {
                key: resolve(*key),
                value: resolve(*value),
            })),
            Self::Set(ty) => Type::Set(Box::new(resolve(*ty))),
            Self::Record { fields, rest } => Type::Record(RecordType {
                fields: fields
                    .iter()
                    .map(|field| RecordTypeField {
                        name: field.name.clone(),
                        ty: resolve(field.ty),
                        optional: field.optional,
                    })
                    .collect(),
                rest: match rest {
                    Rest::Closed => RecordRest::Closed,
                    Rest::Drop => RecordRest::Drop,
                    Rest::Keep(ty) => RecordRest::Keep(Box::new(resolve(*ty))),
                },
            }),
            Self::Function { params, result } => Type::Function(Box::new(Signature {
                params: params
                    .iter()
                    .map(|param| Param {
                        name: param.name.clone(),
                        ty: resolve(param.ty),
                        optional: param.optional,
                    })
                    .collect(),
                result: resolve(*result),
            })),
            Self::Task(ty) => Type::Task(Box::new(resolve(*ty))),
            Self::Union(items) => Type::Union(items.iter().map(|i| resolve(*i)).collect()),
        }
    }

    fn children(&self) -> Vec<usize> {
        match self {
            Self::Leaf(_) => Vec::new(),
            Self::Tuple(items) | Self::Union(items) => items.clone(),
            Self::List(ty) | Self::Set(ty) | Self::Task(ty) => vec![*ty],
            Self::Map { key, value } => vec![*key, *value],
            Self::Record { fields, rest } => fields
                .iter()
                .map(|field| field.ty)
                .chain(match rest {
                    Rest::Keep(ty) => Some(*ty),
                    Rest::Closed | Rest::Drop => None,
                })
                .collect(),
            Self::Function { params, result } => params
                .iter()
                .map(|param| param.ty)
                .chain(std::iter::once(*result))
                .collect(),
        }
    }

    /// The heap bytes the built node allocates itself, its children's
    /// slots included and their own allocations not: each allocation
    /// charged its length and [`ALLOCATION`].
    fn own_bytes(&self) -> u64 {
        fn slots<T>(count: usize) -> u64 {
            ALLOCATION + (count * size_of::<T>()) as u64
        }
        let text = |len: usize| ALLOCATION + len as u64;
        match self {
            Self::Leaf(Type::Enum(values)) => {
                slots::<String>(values.len()) + values.iter().map(|v| text(v.len())).sum::<u64>()
            }
            Self::Leaf(Type::Handle(kind)) => text(kind.len()),
            Self::Leaf(_) => 0,
            Self::Tuple(items) | Self::Union(items) => slots::<Type>(items.len()),
            Self::List(_) | Self::Set(_) | Self::Task(_) => slots::<Type>(1),
            Self::Map { .. } => slots::<MapType>(1),
            Self::Record { fields, rest } => {
                slots::<RecordTypeField>(fields.len())
                    + fields.iter().map(|f| text(f.name.len())).sum::<u64>()
                    + match rest {
                        Rest::Keep(_) => slots::<Type>(1),
                        Rest::Closed | Rest::Drop => 0,
                    }
            }
            Self::Function { params, .. } => {
                slots::<Signature>(1)
                    + slots::<Param>(params.len())
                    + params
                        .iter()
                        .map(|p| text(p.name.as_str().len()))
                        .sum::<u64>()
            }
        }
    }
}

/// What one heap allocation costs beyond its bytes: the allocator's header
/// and alignment.
const ALLOCATION: u64 = 16;

#[derive(Default)]
pub(super) struct TypeTable<'a> {
    pub(super) nodes: Vec<TypeNode>,
    indices: HashMap<&'a Type, usize>,
}

impl<'a> TypeTable<'a> {
    pub(super) fn intern(&mut self, ty: &'a Type) -> usize {
        if let Some(index) = self.indices.get(ty) {
            return *index;
        }
        let node = TypeNode::of(ty, |child| self.intern(child));
        let index = self.nodes.len();
        self.nodes.push(node);
        self.indices.insert(ty, index);
        index
    }
}

/// A decoded type table. Every reference to it is built as its own tree,
/// which is what the kernel's types are, so each is charged the heap bytes
/// that tree really allocates; the table itself is never built.
pub(super) struct Types {
    nodes: Vec<TypeNode>,
    /// The heap bytes of each node's built tree.
    bytes: Vec<u64>,
}

impl Types {
    /// Backward-only references exclude cycles, and the depth bound keeps
    /// every build and every recursive Type operation on it bounded.
    pub(super) fn new<E: Error>(nodes: Vec<TypeNode>) -> Result<Self, E> {
        let mut bytes: Vec<u64> = Vec::with_capacity(nodes.len());
        let mut depths: Vec<u32> = Vec::with_capacity(nodes.len());
        for node in &nodes {
            if let TypeNode::Leaf(ty) = node
                && !matches!(
                    ty,
                    Type::Any
                        | Type::Json
                        | Type::Null
                        | Type::Absent
                        | Type::Bool
                        | Type::Int
                        | Type::Float
                        | Type::Number
                        | Type::Text
                        | Type::Bytes
                        | Type::Timestamp
                        | Type::Enum(_)
                        | Type::Error
                        | Type::Handle(_)
                )
            {
                return Err(E::custom("shared type leaf contains nested types"));
            }
            let mut size = node.own_bytes();
            let mut depth = 1;
            for child in node.children() {
                let child_bytes = bytes
                    .get(child)
                    .ok_or_else(|| E::custom("unknown or forward shared signature type"))?;
                size = size.saturating_add(*child_bytes);
                depth = depth.max(depths[child] + 1);
            }
            if depth > lash_vm_protocol::DecodeLimits::standard().max_depth {
                return Err(E::custom("decoded signatures exceed type depth bound"));
            }
            bytes.push(size);
            depths.push(depth);
        }
        Ok(Self { nodes, bytes })
    }

    /// Charge the tree `index` is built as, its root slot included, against
    /// `remaining`, before anything is built.
    pub(super) fn charge<E: Error>(&self, index: usize, remaining: &mut u64) -> Result<(), E> {
        let size = self
            .bytes
            .get(index)
            .ok_or_else(|| E::custom("unknown shared signature type"))?;
        *remaining = remaining
            .checked_sub(size.saturating_add(size_of::<Type>() as u64))
            .ok_or_else(|| E::custom("decoded signatures exceed 64 MiB"))?;
        Ok(())
    }

    /// The type `index` names, built whole. Only a charged index is built.
    pub(super) fn get(&self, index: usize) -> Type {
        self.nodes[index].build(|child| self.get(child))
    }
}
