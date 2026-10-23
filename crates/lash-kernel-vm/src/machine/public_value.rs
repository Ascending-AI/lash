//! Dynamic fitting at a public function boundary, before private ABI adaptation.
use super::{Eval, KernelMachine, fault};
use crate::compile::Executable;
use lash_kernel_doc::{
    Datum, Value,
    arena::{Body, Chunk},
};
use lash_vocabulary::{Type, TypeMismatch};

impl KernelMachine {
    fn public_value_type(&mut self, value: Value) -> Eval<Type> {
        Ok(match value {
            Value::Null => Type::Null,
            Value::Absent => Type::Absent,
            Value::Bool(_) => Type::Bool,
            Value::Int(_) | Value::BigInt(_) => Type::Int,
            Value::Float(_) => Type::Float,
            Value::Text(_) | Value::LongText(_) => Type::Text,
            Value::Bytes(_) => Type::Bytes,
            Value::Timestamp(_) | Value::BigTimestamp(_) => Type::Timestamp,
            Value::Error(_) => Type::Error,
            Value::Closure(_) | Value::Function(_) => {
                Type::Function(Box::new(lash_vocabulary::Signature {
                    params: vec![],
                    result: Type::Any,
                }))
            }
            Value::Task(_) | Value::TaskRef(_) => Type::Task(Box::new(Type::Any)),
            Value::Tuple(_) => Type::Tuple(vec![]),
            Value::List(_) => Type::List(Box::new(Type::Any)),
            Value::Set(_) => Type::Set(Box::new(Type::Any)),
            Value::Map(_) => Type::Map(Box::new(lash_vocabulary::MapType {
                key: Type::Any,
                value: Type::Any,
            })),
            Value::Record(_) => Type::Record(lash_vocabulary::RecordType {
                fields: vec![],
                rest: Some(Box::new(Type::Any)),
            }),
            Value::ObjectRef(_) => Type::Any,
            Value::Handle(_) => lash_kernel_doc::datum_type(&self.copy_datum(&value)?),
        })
    }

    pub(super) fn public_value_fits(
        &mut self,
        exe: &Executable,
        value: Value,
        ty: &Type,
        depth: usize,
    ) -> Eval<Option<TypeMismatch>> {
        if depth == 0 {
            return self.decode_refusal(lash_kernel_doc::DecodeRefusal::DepthLimit {
                limit: self.limits.value_depth(),
            });
        }
        if matches!(ty, Type::Any) {
            return Ok(None);
        }
        self.warm_reachable(&value)?;
        let actual = self.public_value_type(value)?;
        let mismatch = || Some(TypeMismatch::new(ty.clone(), actual.clone()));
        if let Type::Union(types) = ty {
            for ty in types {
                if self.public_value_fits(exe, value, ty, depth - 1)?.is_none() {
                    return Ok(None);
                }
            }
            return Ok(mismatch());
        }
        if let Type::Task(_) = ty {
            return Ok((!matches!(value, Value::Task(_) | Value::TaskRef(_)))
                .then(mismatch)
                .flatten());
        }
        if matches!(ty, Type::Json)
            && let Value::Error(handle) = value
        {
            let data = {
                let arena = self.arena.borrow();
                let Body::Error(error) = arena.body(handle).map_err(|e| fault(&e.to_string()))?
                else {
                    return Err(fault("error handle has another kind").into());
                };
                error.data
            };
            return Ok(self
                .public_value_fits(exe, data, ty, depth - 1)?
                .map(|error| error.within("data")));
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
                        return Ok(mismatch());
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
                    let Some(presentation) = lash_kernel_doc::snapshot::closure_at(
                        document,
                        &identity.site.site,
                        identity.callable.link,
                        exe.helpers(),
                        exe.selection(),
                    )
                    .and_then(|closure| closure.presentation.as_ref()) else {
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
                    return Ok(mismatch());
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
                return Ok(mismatch());
            };
            let Some(document) = exe.instances.get(&callable.link) else {
                return Ok(mismatch());
            };
            let signature = site
                .as_ref()
                .and_then(|site| {
                    lash_kernel_doc::snapshot::closure_at(
                        document,
                        &site.site,
                        callable.link,
                        exe.helpers(),
                        exe.selection(),
                    )
                })
                .and_then(|closure| closure.presentation.as_ref())
                .map(|presentation| presentation.signature.clone())
                .or_else(|| {
                    document.exports.values().find_map(|export| match export {
                        lash_kernel_doc::Export::Function(export)
                            if export.function == callable.function =>
                        {
                            Some(export.signature.clone())
                        }
                        _ => None,
                    })
                })
                .or_else(|| {
                    document
                        .functions
                        .get(&callable.function)
                        .map(|definition| definition.signature.clone())
                });
            let signature = signature.or_else(|| {
                exe.admits_callable(&callable)
                    .then(|| {
                        exe.helpers()
                            .registered(&callable.function)
                            .map(|registered| registered.header().signature.clone())
                    })
                    .flatten()
            });
            return Ok(if let Some(provided) = signature {
                lash_vocabulary::type_relations::serves(&provided, expected).map(|_| {
                    TypeMismatch::new(ty.clone(), Type::Function(Box::new(provided.clone())))
                })
            } else {
                mismatch()
            });
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
                (Type::Json, Value::Tuple(_) | Value::List(_) | Value::Set(_)) => {
                    let stride = if matches!(value, Value::Set(_)) { 2 } else { 1 };
                    for (index, value) in members.into_iter().step_by(stride).enumerate() {
                        if let Some(error) = self.public_value_fits(exe, value, ty, depth - 1)? {
                            return Ok(Some(error.within(&index.to_string())));
                        }
                    }
                    return Ok(None);
                }
                (Type::Json, Value::Record(_)) => {
                    for pair in members.as_chunks::<2>().0 {
                        let Datum::Text(name) = self.copy_datum(&pair[0])? else {
                            return Err(fault("record key is not text").into());
                        };
                        if let Some(error) = self.public_value_fits(exe, pair[1], ty, depth - 1)? {
                            return Ok(Some(error.within(&name)));
                        }
                    }
                    return Ok(None);
                }
                (Type::Json, Value::Map(_)) => {
                    for (index, pair) in members.as_chunks::<2>().0.iter().enumerate() {
                        for (part, value) in pair.iter().enumerate() {
                            if let Some(error) =
                                self.public_value_fits(exe, *value, ty, depth - 1)?
                            {
                                return Ok(Some(
                                    error.within(&part.to_string()).within(&index.to_string()),
                                ));
                            }
                        }
                    }
                    return Ok(None);
                }
                (Type::Tuple(types), Value::Tuple(_) | Value::List(_)) => {
                    if types.len() != members.len() {
                        return Ok(mismatch());
                    }
                    for (index, (value, ty)) in members.into_iter().zip(types).enumerate() {
                        if let Some(error) = self.public_value_fits(exe, value, ty, depth - 1)? {
                            return Ok(Some(error.within(&index.to_string())));
                        }
                    }
                    return Ok(None);
                }
                (Type::List(ty), Value::List(_)) | (Type::Set(ty), Value::Set(_)) => {
                    let stride = if matches!(value, Value::Set(_)) { 2 } else { 1 };
                    for (index, value) in members.into_iter().step_by(stride).enumerate() {
                        if let Some(error) = self.public_value_fits(exe, value, ty, depth - 1)? {
                            return Ok(Some(error.within(&index.to_string())));
                        }
                    }
                    return Ok(None);
                }
                (Type::Map(types), Value::Map(_)) => {
                    for (index, pair) in members.as_chunks::<2>().0.iter().enumerate() {
                        for (part, value, ty) in
                            [("0", pair[0], &types.key), ("1", pair[1], &types.value)]
                        {
                            if let Some(error) =
                                self.public_value_fits(exe, value, ty, depth - 1)?
                            {
                                let at = if matches!(pair[0], Value::Text(_) | Value::LongText(_)) {
                                    let Datum::Text(name) = self.copy_datum(&pair[0])? else {
                                        return Err(fault("text key has another kind").into());
                                    };
                                    error.within(&name)
                                } else {
                                    error.within(part).within(&index.to_string())
                                };
                                return Ok(Some(at));
                            }
                        }
                    }
                    return Ok(None);
                }
                (Type::Record(record), Value::Record(_)) => {
                    let mut seen = std::collections::BTreeSet::new();
                    for pair in members.as_chunks::<2>().0 {
                        let Datum::Text(name) = self.copy_datum(&pair[0])? else {
                            return Ok(mismatch());
                        };
                        let ty = record
                            .fields
                            .iter()
                            .find(|field| field.name == name)
                            .map(|field| &field.ty)
                            .or(record.rest.as_deref());
                        let Some(ty) = ty else {
                            return Ok(Some(
                                TypeMismatch::new(Type::Absent, self.public_value_type(pair[1])?)
                                    .within(&name),
                            ));
                        };
                        if let Some(error) = self.public_value_fits(exe, pair[1], ty, depth - 1)? {
                            return Ok(Some(error.within(&name)));
                        }
                        seen.insert(name);
                    }
                    return Ok(record
                        .fields
                        .iter()
                        .find(|f| !f.optional && !seen.contains(&f.name))
                        .map(|f| TypeMismatch::new(f.ty.clone(), Type::Absent).within(&f.name)));
                }
                _ => {}
            }
        }
        if matches!(
            value,
            Value::Closure(_) | Value::Task(_) | Value::TaskRef(_) | Value::ObjectRef(_)
        ) {
            return Ok(mismatch());
        }
        let datum = match self.copy_datum(&value) {
            Ok(datum) => datum,
            Err(crate::data::CopyError::Raised(error)) => return Err(error.into()),
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
        match decoder.decode(&datum, ty) {
            Ok(_) => Ok(None),
            Err(lash_kernel_doc::DecodeRefusal::Type { mismatch }) => Ok(Some(mismatch)),
            Err(other) => self.decode_refusal(other),
        }
    }
}
