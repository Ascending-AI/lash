//! An authored call of a typed build helper's body holds its arguments
//! to the helper's signature, as an export's are (`function_argument`),
//! and the value the body returns to its result (`entry_type_mismatch`
//! at `output`): a dialect helper's body may rely on the types it
//! declares, and its caller on the type it returns.
//!
//! A value fits a type as the decoder admits its copy, member by member
//! where it holds members, so a function or a task among them is judged
//! by its kind rather than refused as data. A function fits a function
//! type when the signature it presents serves it.

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::snapshot::closure_at;
use lash_kernel_doc::{Datum, Decoder, Export, FunctionRepresentation, Value};
use lash_kernel_value::type_relations::serves;
use lash_kernel_value::{EntryTypeMismatch, RecordRest, Signature, Type, TypeMismatch};

use super::datum::copy_in;
use super::{Run, Vm, fault, read};
use crate::bytecode::op::Op;
use crate::bytecode::program::Checked;
use crate::heap::Heap;

impl<H: Heap> Vm<H> {
    /// Raises `function_argument` when one of the top `argc` operands does
    /// not fit `checked`'s parameter at its place.
    pub(super) fn check_arguments(&mut self, checked: &Checked, argc: usize) -> Run<()> {
        let Signature { params, .. } = &checked.signature;
        if !(checked.signature.required()..=params.len()).contains(&argc) {
            return Err(self.error(
                "arity",
                "arguments do not fit the imported function signature",
            ));
        }
        let from = self.cx.stack.len() - argc;
        for (param, at) in params.iter().zip(from..).take(argc) {
            let value = self.cx.stack[at];
            if param.optional && matches!(value, Value::Absent) {
                continue;
            }
            if let Some(mismatch) = self.fits(value, &param.ty, self.value_depth)? {
                let payload = EntryTypeMismatch {
                    callable: checked.callable,
                    mismatch: mismatch.within(param.name.as_str()).within("input"),
                };
                return Err(self.typed_error("function_argument", &payload));
            }
        }
        Ok(())
    }

    /// Why `value`, returned to an authored call of a typed build
    /// helper's body, does not fit the helper's declared result: the
    /// caller's frame stands below the returning one, at the call.
    pub(super) fn unfit_result(&mut self, value: Value) -> Run<Option<EntryTypeMismatch>> {
        let Some(caller) = self.cx.frames.len().checked_sub(2) else {
            return Ok(None);
        };
        let frame = &self.cx.frames[caller];
        let unit = self.program.unit(frame.unit);
        let checked = match (frame.pc as usize)
            .checked_sub(1)
            .and_then(|pc| unit.ops.get(pc))
        {
            Some(Op::Call(site)) => unit.calls[*site as usize].checked.clone(),
            _ => None,
        };
        let Some(checked) = checked.filter(|checked| checked.signature.result != Type::Any) else {
            return Ok(None);
        };
        Ok(self
            .fits(value, &checked.signature.result, self.value_depth)?
            .map(|mismatch| EntryTypeMismatch {
                callable: checked.callable,
                mismatch: mismatch.within("output"),
            }))
    }

    /// The signature function value `value` presents: a closure's
    /// dialect presentation, else its definition's or an admitted
    /// helper's. A tuple presents one only as a dialect token: its first
    /// member is the token's tag and the closure it holds stands where
    /// its presentation says.
    fn presented(&mut self, value: Value) -> Run<Option<Signature>> {
        let value = match value {
            Value::Tuple(tuple) => match self.token(tuple)? {
                Some(closure) => closure,
                None => return Ok(None),
            },
            other => other,
        };
        let (callable, site) = match value {
            Value::Closure(handle) => {
                let (identity, _, _) = self.heap.arena().closure(handle).map_err(read)?;
                (identity.callable, Some(identity.site.site.clone()))
            }
            Value::Function(handle) => (self.heap.arena().callable(handle).map_err(read)?, None),
            _ => return Ok(None),
        };
        let program = &self.crossing.program;
        let Some(document) = program
            .instances
            .get(&callable.link)
            .and_then(|instance| program.documents.get(&instance.document))
        else {
            return Ok(None);
        };
        let helpers = program.helpers();
        let presented = site
            .and_then(|site| closure_at(document, &site, helpers))
            .and_then(|closure| closure.presentation.as_ref())
            .map(|presentation| presentation.signature.clone());
        let exported = || {
            document.exports.values().find_map(|export| match export {
                Export::Function(export) if export.function == callable.function => {
                    Some(export.signature.clone())
                }
                _ => None,
            })
        };
        let defined = || {
            document
                .functions
                .get(&callable.function)
                .map(|definition| definition.signature.clone())
        };
        let admitted = || {
            helpers
                .admits(&document.library, &callable.function)
                .then(|| helpers.registered(&callable.function))
                .flatten()
                .map(|registered| registered.header().signature.clone())
        };
        Ok(presented
            .or_else(exported)
            .or_else(defined)
            .or_else(admitted))
    }

    /// The closure dialect token `tuple` holds, if it is one.
    fn token(&mut self, tuple: ArenaHandle) -> Run<Option<Value>> {
        let members = self.heap.arena().sequence(tuple).map_err(read)?.to_vec();
        let Some(first) = members.first().copied() else {
            return Ok(None);
        };
        for (index, member) in members.iter().enumerate() {
            let Value::Closure(handle) = member else {
                continue;
            };
            let identity = self.heap.arena().closure(*handle).map_err(read)?.0.clone();
            let program = &self.crossing.program;
            let token = program
                .instances
                .get(&identity.callable.link)
                .and_then(|instance| program.documents.get(&instance.document))
                .and_then(|document| closure_at(document, &identity.site.site, program.helpers()))
                .and_then(|closure| closure.presentation.as_ref())
                .and_then(|presentation| match &presentation.representation {
                    FunctionRepresentation::Token { tag, callable } => {
                        (*callable as usize == index).then(|| tag.clone())
                    }
                    FunctionRepresentation::Bare => None,
                });
            if let Some(tag) = token
                && matches!(first, Value::Text(_) | Value::LongText(_))
                && self.text_of(&first)? == tag
            {
                return Ok(Some(*member));
            }
        }
        Ok(None)
    }

    /// The raise of an error of `kind` whose data is `payload`'s
    /// diagnostic datum.
    pub(super) fn typed_error(
        &mut self,
        kind: &str,
        payload: &impl serde::Serialize,
    ) -> super::Flow {
        let datum = match lash_kernel_doc::diagnostic_datum(payload) {
            Ok(datum) => datum,
            Err(error) => return fault(error.to_string()),
        };
        let mark = self.held.len();
        let built = copy_in(self, &datum).and_then(|data| {
            self.held.push(data);
            self.new_error(kind, kind, data)
        });
        self.held.truncate(mark);
        match built {
            Ok(value) => super::Flow::Raise(value),
            Err(flow) => flow,
        }
    }

    /// Why `value` does not fit `ty`, if it does not.
    fn fits(&mut self, value: Value, ty: &Type, depth: usize) -> Run<Option<TypeMismatch>> {
        if depth == 0 {
            return Err(self.error(
                "too_deep",
                &format!(
                    "a value checked at a call exceeds the configured {}-level value depth",
                    self.value_depth
                ),
            ));
        }
        if matches!(ty, Type::Any) {
            return Ok(None);
        }
        let mismatch = |vm: &mut Self| -> Run<Option<TypeMismatch>> {
            Ok(Some(TypeMismatch::new(ty.clone(), vm.kind_of(value)?)))
        };
        if let Type::Union(types) = ty {
            for ty in types {
                if self.fits(value, ty, depth - 1)?.is_none() {
                    return Ok(None);
                }
            }
            return mismatch(self);
        }
        match (ty, value) {
            (Type::Task(_), Value::Task(_) | Value::TaskRef(_)) => return Ok(None),
            (Type::Task(_), _) => return mismatch(self),
            (Type::Function(expected), _) => {
                let Some(provided) = self.presented(value)? else {
                    return mismatch(self);
                };
                return Ok(serves(&provided, expected)
                    .map(|_| TypeMismatch::new(ty.clone(), Type::Function(Box::new(provided)))));
            }
            (Type::Json, Value::Error(error)) => {
                let data = self.error_data(error)?;
                return Ok(self
                    .fits(data, ty, depth - 1)?
                    .map(|mismatch| mismatch.within("data")));
            }
            (Type::Json, Value::Tuple(handle) | Value::List(handle)) => {
                return self.each(handle, false, 1, ty, depth);
            }
            (Type::Json, Value::Set(handle)) => return self.each(handle, true, 2, ty, depth),
            (Type::Json, Value::Record(handle)) => return self.fields(handle, None, ty, depth),
            (Type::Json, Value::Map(handle)) => return self.pairs(handle, ty, ty, depth),
            (Type::Tuple(types), Value::Tuple(handle) | Value::List(handle)) => {
                let members = self.heap.arena().sequence(handle).map_err(read)?.to_vec();
                if members.len() != types.len() {
                    return mismatch(self);
                }
                for (index, (member, ty)) in members.into_iter().zip(types).enumerate() {
                    if let Some(error) = self.fits(member, ty, depth - 1)? {
                        return Ok(Some(error.within(&index.to_string())));
                    }
                }
                return Ok(None);
            }
            (Type::List(item), Value::List(handle)) => {
                return self.each(handle, false, 1, item, depth);
            }
            (Type::Set(item), Value::Set(handle)) => {
                return self.each(handle, true, 2, item, depth);
            }
            (Type::Map(map), Value::Map(handle)) => {
                return self.pairs(handle, &map.key, &map.value, depth);
            }
            (Type::Record(record), Value::Record(handle)) => {
                return self.fields(handle, Some(record), ty, depth);
            }
            (_, Value::Closure(_) | Value::Task(_) | Value::TaskRef(_) | Value::ObjectRef(_)) => {
                return mismatch(self);
            }
            _ => {}
        }
        let datum = self.copy_checked(value)?;
        let document = &self.document;
        let program = &self.program;
        let decoder = Decoder {
            max_depth: self.value_depth,
            policy: document.numbers,
            declared: &|callable| program.callable(callable),
        };
        match decoder.decode(&datum, ty) {
            Ok(_) => Ok(None),
            Err(lash_kernel_doc::DecodeRefusal::Type { mismatch }) => Ok(Some(mismatch)),
            Err(other) => Err(self.error("decode", &other.to_string())),
        }
    }

    /// Each member of the sequence or set `handle` against `ty`.
    fn each(
        &mut self,
        handle: ArenaHandle,
        table: bool,
        stride: usize,
        ty: &Type,
        depth: usize,
    ) -> Run<Option<TypeMismatch>> {
        let arena = self.heap.arena();
        let members = if table {
            arena.entries(handle)
        } else {
            arena.sequence(handle)
        }
        .map_err(read)?
        .iter()
        .step_by(stride)
        .copied()
        .collect::<Vec<_>>();
        for (index, member) in members.into_iter().enumerate() {
            if let Some(error) = self.fits(member, ty, depth - 1)? {
                return Ok(Some(error.within(&index.to_string())));
            }
        }
        Ok(None)
    }

    /// Each key and value of the map `handle` against `key` and `value`.
    fn pairs(
        &mut self,
        handle: ArenaHandle,
        key: &Type,
        value: &Type,
        depth: usize,
    ) -> Run<Option<TypeMismatch>> {
        let entries = self.heap.arena().entries(handle).map_err(read)?.to_vec();
        for (index, pair) in entries.as_chunks::<2>().0.iter().enumerate() {
            for (part, member, ty) in [("0", pair[0], key), ("1", pair[1], value)] {
                if let Some(error) = self.fits(member, ty, depth - 1)? {
                    let at = match pair[0] {
                        Value::Text(_) | Value::LongText(_) => {
                            error.within(&self.text_of(&pair[0])?)
                        }
                        _ => error.within(part).within(&index.to_string()),
                    };
                    return Ok(Some(at));
                }
            }
        }
        Ok(None)
    }

    /// Each field of the record `handle` against `record`'s, or each
    /// against `json` when no record type is declared.
    fn fields(
        &mut self,
        handle: ArenaHandle,
        record: Option<&lash_kernel_value::RecordType>,
        json: &Type,
        depth: usize,
    ) -> Run<Option<TypeMismatch>> {
        let entries = self.heap.arena().entries(handle).map_err(read)?.to_vec();
        let mut seen = std::collections::BTreeSet::new();
        for pair in entries.as_chunks::<2>().0 {
            let name = self.text_of(&pair[0])?;
            let ty = match record {
                None => Some(json),
                Some(record) => record
                    .fields
                    .iter()
                    .find(|field| field.name == name)
                    .map(|field| &field.ty)
                    .or(record.rest.kept_type()),
            };
            let Some(ty) = ty else {
                if record.is_some_and(|record| matches!(record.rest, RecordRest::Drop)) {
                    continue;
                }
                return Ok(Some(
                    TypeMismatch::new(Type::Absent, self.kind_of(pair[1])?).within(&name),
                ));
            };
            if let Some(error) = self.fits(pair[1], ty, depth - 1)? {
                return Ok(Some(error.within(&name)));
            }
            seen.insert(name);
        }
        Ok(record.and_then(|record| {
            record
                .fields
                .iter()
                .find(|field| !field.optional && !seen.contains(&field.name))
                .map(|field| TypeMismatch::new(field.ty.clone(), Type::Absent).within(&field.name))
        }))
    }

    /// The type a mismatch names `value` by: its kind.
    fn kind_of(&mut self, value: Value) -> Run<Type> {
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
            Value::Closure(_) | Value::Function(_) => Type::Function(Box::new(Signature {
                params: vec![],
                result: Type::Any,
            })),
            Value::Task(_) | Value::TaskRef(_) => Type::Task(Box::new(Type::Any)),
            Value::Tuple(_) => Type::Tuple(vec![]),
            Value::List(_) => Type::List(Box::new(Type::Any)),
            Value::Set(_) => Type::Set(Box::new(Type::Any)),
            Value::Map(_) => Type::Map(Box::new(lash_kernel_value::MapType {
                key: Type::Any,
                value: Type::Any,
            })),
            Value::Record(_) => Type::Record(lash_kernel_value::RecordType {
                fields: vec![],
                rest: RecordRest::Keep(Box::new(Type::Any)),
            }),
            Value::ObjectRef(_) => Type::Any,
            Value::Handle(_) => lash_kernel_doc::datum_type(&self.copy_checked(value)?),
        })
    }

    /// `value`'s copy, read for a check: nothing billed.
    fn copy_checked(&mut self, value: Value) -> Run<Datum> {
        match crate::data::copy_out(self.heap.arena(), self.value_depth, &value) {
            Ok(datum) => Ok(datum),
            Err(crate::data::CopyError::Raised(raised)) => {
                Err(self.error(raised.kind, &raised.message))
            }
            Err(crate::data::CopyError::Cold { object, chunk }) => {
                Err(super::Flow::Cold { object, chunk })
            }
            Err(crate::data::CopyError::Heap(error)) => Err(fault(error.to_string())),
        }
    }

    fn error_data(&self, error: ArenaHandle) -> Run<Value> {
        match self.heap.arena().body(error).map_err(read)? {
            Body::Error(error) => Ok(error.data),
            Body::Cold(_) => Err(super::Flow::Cold {
                object: error,
                chunk: 0,
            }),
            _ => Err(fault("an error names another body")),
        }
    }
}
