//! Dynamic fitting at a public function boundary, before private ABI adaptation.
use super::{Eval, KernelMachine, fault};
use crate::compile::Executable;
use lash_kernel_doc::{
    Datum, Value,
    arena::{Body, Chunk},
};
use lash_vocabulary::Type;

impl KernelMachine {
    pub(super) fn public_value_fits(
        &mut self,
        exe: &Executable,
        value: Value,
        ty: &Type,
        depth: usize,
    ) -> Eval<bool> {
        if depth == 0 {
            return Ok(false);
        }
        if matches!(ty, Type::Any) {
            return Ok(true);
        }
        if let Type::Union(types) = ty {
            for ty in types {
                if self.public_value_fits(exe, value, ty, depth - 1)? {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        self.warm_reachable(&value)?;
        if let Type::Task(_) = ty {
            return Ok(matches!(value, Value::Task(_) | Value::TaskRef(_)));
        }
        if let Type::Function(expected) = ty {
            // A dialect token is callable only through its defining closure's
            // presentation; an arbitrary tuple with a function in it is data.
            let value = if let Value::Tuple(handle) = value {
                let members = {
                    let arena = self.arena.borrow();
                    let Body::Tuple(sequence) =
                        arena.body(handle).map_err(|e| fault(&e.to_string()))?
                    else {
                        return Ok(false);
                    };
                    sequence
                        .chunks
                        .iter()
                        .flat_map(|chunk| match chunk {
                            Chunk::Resident(values) => values.to_vec(),
                            Chunk::Cold { .. } => Vec::new(),
                        })
                        .collect::<Vec<_>>()
                };
                let mut found = None;
                for (index, member) in members.iter().enumerate() {
                    let Value::Closure(handle) = member else {
                        continue;
                    };
                    let identity = {
                        let arena = self.arena.borrow();
                        let Body::Closure { identity, .. } =
                            arena.body(*handle).map_err(|e| fault(&e.to_string()))?
                        else {
                            continue;
                        };
                        identity.clone()
                    };
                    let Some(document) = exe.instances.get(&identity.callable.link) else {
                        continue;
                    };
                    let Some(presentation) =
                        lash_kernel_doc::snapshot::closure_at(document, &identity.site.site)
                            .and_then(|closure| closure.presentation.as_ref())
                    else {
                        continue;
                    };
                    let lash_kernel_doc::FunctionRepresentation::Token { tag, callable } =
                        &presentation.representation
                    else {
                        continue;
                    };
                    if *callable as usize == index
                        && let Some(first) = members.first()
                        && self.copy_datum(first)? == Datum::Text(tag.clone())
                    {
                        found = Some(*member);
                        break;
                    }
                }
                let Some(value) = found else {
                    return Ok(false);
                };
                value
            } else {
                value
            };
            let callable = match value {
                Value::Function(handle) | Value::Closure(handle) => {
                    let arena = self.arena.borrow();
                    match arena.body(handle).map_err(|e| fault(&e.to_string()))? {
                        Body::Function(callable) => Some((**callable, None)),
                        Body::Closure { identity, .. } => {
                            Some((identity.callable, Some(identity.site.clone())))
                        }
                        _ => None,
                    }
                }
                _ => None,
            };
            let Some((callable, site)) = callable else {
                return Ok(false);
            };
            let Some(document) = exe.instances.get(&callable.link) else {
                return Ok(false);
            };
            let signature = site
                .as_ref()
                .and_then(|site| lash_kernel_doc::snapshot::closure_at(document, &site.site))
                .and_then(|closure| closure.presentation.as_ref())
                .map(|presentation| &presentation.signature)
                .or_else(|| {
                    document.exports.values().find_map(|export| match export {
                        lash_kernel_doc::Export::Function(export)
                            if export.function == callable.function =>
                        {
                            Some(&export.signature)
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    document
                        .functions
                        .get(&callable.function)
                        .map(|definition| &definition.signature)
                });
            return Ok(signature.is_some_and(|provided| {
                lash_vocabulary::type_relations::serves(provided, expected).is_none()
            }));
        }
        let members = match value {
            Value::Tuple(handle)
            | Value::List(handle)
            | Value::Map(handle)
            | Value::Set(handle)
            | Value::Record(handle) => {
                let arena = self.arena.borrow();
                let chunks = match arena.body(handle).map_err(|e| fault(&e.to_string()))? {
                    Body::Tuple(sequence) | Body::List(sequence) => &sequence.chunks,
                    Body::Map(table) | Body::Set(table) | Body::Record(table) => &table.chunks,
                    _ => return Err(fault("public value's handle has another kind").into()),
                };
                Some(
                    chunks
                        .iter()
                        .flat_map(|chunk| match chunk {
                            Chunk::Resident(values) => values.to_vec(),
                            Chunk::Cold { .. } => Vec::new(),
                        })
                        .collect::<Vec<_>>(),
                )
            }
            _ => None,
        };
        if let Some(members) = members {
            match (ty, value) {
                (Type::Tuple(types), Value::Tuple(_) | Value::List(_)) => {
                    if types.len() != members.len() {
                        return Ok(false);
                    }
                    for (value, ty) in members.into_iter().zip(types) {
                        if !self.public_value_fits(exe, value, ty, depth - 1)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                (Type::List(ty), Value::List(_)) | (Type::Set(ty), Value::Set(_)) => {
                    let stride = if matches!(value, Value::Set(_)) { 2 } else { 1 };
                    for value in members.into_iter().step_by(stride) {
                        if !self.public_value_fits(exe, value, ty, depth - 1)? {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                (Type::Map(types), Value::Map(_)) => {
                    for pair in members.chunks_exact(2) {
                        if !self.public_value_fits(exe, pair[0], &types.key, depth - 1)?
                            || !self.public_value_fits(exe, pair[1], &types.value, depth - 1)?
                        {
                            return Ok(false);
                        }
                    }
                    return Ok(true);
                }
                (Type::Record(record), Value::Record(_)) => {
                    let mut seen = std::collections::BTreeSet::new();
                    for pair in members.chunks_exact(2) {
                        let Datum::Text(name) = self.copy_datum(&pair[0])? else {
                            return Ok(false);
                        };
                        let ty = record
                            .fields
                            .iter()
                            .find(|field| field.name == name)
                            .map(|field| &field.ty)
                            .or(record.rest.as_deref());
                        let Some(ty) = ty else {
                            return Ok(false);
                        };
                        if !self.public_value_fits(exe, pair[1], ty, depth - 1)? {
                            return Ok(false);
                        }
                        seen.insert(name);
                    }
                    return Ok(record
                        .fields
                        .iter()
                        .all(|field| field.optional || seen.contains(&field.name)));
                }
                _ => {}
            }
        }
        if matches!(
            value,
            Value::Closure(_) | Value::Task(_) | Value::TaskRef(_) | Value::ObjectRef(_)
        ) {
            return Ok(false);
        }
        let datum = match self.copy_datum(&value) {
            Ok(datum) => datum,
            Err(crate::data::CopyError::Raised(_)) => return Ok(false),
            Err(crate::data::CopyError::Heap(error)) => return Err(error.into()),
        };
        let decoder = lash_kernel_doc::Decoder {
            max_depth: self.limits.value_depth(),
            policy: exe
                .instances
                .get(&self.current_link())
                .map_or(self.program.document.numbers, |document| document.numbers),
            declared: &|callable| {
                exe.instances
                    .get(&callable.link)
                    .is_some_and(|document| document.functions.contains_key(&callable.function))
            },
        };
        Ok(decoder.decode(&datum, ty).is_ok())
    }
}
