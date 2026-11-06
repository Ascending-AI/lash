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

    fn build<E: Error>(self, mut resolve: impl FnMut(usize) -> Result<Type, E>) -> Result<Type, E> {
        Ok(match self {
            Self::Leaf(ty) => match ty {
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
                | Type::Handle(_) => ty,
                _ => return Err(E::custom("shared type leaf contains nested types")),
            },
            Self::Tuple(items) => {
                Type::Tuple(items.into_iter().map(resolve).collect::<Result<_, _>>()?)
            }
            Self::List(ty) => Type::List(Box::new(resolve(ty)?)),
            Self::Map { key, value } => Type::Map(Box::new(MapType {
                key: resolve(key)?,
                value: resolve(value)?,
            })),
            Self::Set(ty) => Type::Set(Box::new(resolve(ty)?)),
            Self::Record { fields, rest } => Type::Record(RecordType {
                fields: fields
                    .into_iter()
                    .map(|field| {
                        Ok(RecordTypeField {
                            name: field.name,
                            ty: resolve(field.ty)?,
                            optional: field.optional,
                        })
                    })
                    .collect::<Result<_, E>>()?,
                rest: match rest {
                    Rest::Closed => RecordRest::Closed,
                    Rest::Drop => RecordRest::Drop,
                    Rest::Keep(ty) => RecordRest::Keep(Box::new(resolve(ty)?)),
                },
            }),
            Self::Function { params, result } => Type::Function(Box::new(Signature {
                params: params
                    .into_iter()
                    .map(|param| {
                        Ok(Param {
                            name: param.name,
                            ty: resolve(param.ty)?,
                            optional: param.optional,
                        })
                    })
                    .collect::<Result<_, E>>()?,
                result: resolve(result)?,
            })),
            Self::Task(ty) => Type::Task(Box::new(resolve(ty)?)),
            Self::Union(items) => {
                Type::Union(items.into_iter().map(resolve).collect::<Result<_, _>>()?)
            }
        })
    }
}

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

fn charge(value: &serde_json::Value) -> u64 {
    128 + match value {
        serde_json::Value::String(s) => s.len() as u64,
        serde_json::Value::Array(v) => v.iter().map(charge).sum(),
        serde_json::Value::Object(v) => v
            .iter()
            .map(|(k, v)| 128 + k.len() as u64 + charge(v))
            .sum(),
        _ => 0,
    }
}

/// Refuse amplification before cloning any children. Backward-only references
/// exclude cycles; explicit depth checking bounds recursive Type operations.
pub(super) fn expand<E: Error>(
    nodes: Vec<TypeNode>,
    remaining: &mut u64,
) -> Result<(Vec<Type>, Vec<u64>), E> {
    let mut types: Vec<Type> = Vec::with_capacity(nodes.len());
    let mut charges: Vec<u64> = Vec::with_capacity(nodes.len());
    let mut depths: Vec<u32> = Vec::with_capacity(nodes.len());
    let null_charge = charge(&serde_json::to_value(Type::Null).map_err(E::custom)?);
    for node in nodes {
        let mut children = 0u64;
        let mut placeholders = 0;
        let mut depth = 1;
        let skeleton = node.clone().build(|index| {
            let size = charges
                .get(index)
                .ok_or_else(|| E::custom("unknown or forward shared signature type"))?;
            children = children.saturating_add(*size);
            placeholders += null_charge;
            depth = depth.max(depths[index] + 1);
            Ok(Type::Null)
        })?;
        if depth > lash_vm_protocol::DecodeLimits::standard().max_depth {
            return Err(E::custom("expanded signatures exceed type depth bound"));
        }
        let size = charge(&serde_json::to_value(skeleton).map_err(E::custom)?)
            .saturating_sub(placeholders)
            .saturating_add(children);
        *remaining = remaining
            .checked_sub(size)
            .ok_or_else(|| E::custom("expanded signatures exceed 64 MiB"))?;
        types.push(node.build::<E>(|index| Ok(types[index].clone()))?);
        charges.push(size);
        depths.push(depth);
    }
    Ok((types, charges))
}
