//! The bytecode core's crossing: an effect argument walked over the
//! arena with its declared type, each function in it frozen as a
//! [`FunctionValue`] where that type admits one (`K-EFF-002`,
//! `K-CLO-004`). The rules are [`super`]'s; this reads the core's heap.
//!
//! Every read may find a cold chunk: the walk then ends with
//! [`CopyError::Cold`] and the action runs again once it is loaded, so
//! nothing here commits.

use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::arena::{ArenaHandle, Body};
use lash_kernel_doc::{CaptureValue, Datum, ErrorDatum, FunctionRepresentation, FunctionValue};
use lash_kernel_state::arena::Arena;
use lash_kernel_value::names::{CallableId, Name};
use lash_kernel_value::types::Type;

use super::captures::LinkedCatalog;
use super::{ArgumentPath, CrossingRun, Slot, Step, defining_document, step_text};
use crate::data::{CopyError, raised};

/// What a crossing reads of the run besides its arena: the value a free
/// name of `main` stands for in the session (`K-SES-001`), for a closure
/// `main` made.
pub(crate) type Session<'a> = &'a dyn Fn(&Name) -> Option<lash_kernel_doc::Value>;

struct Walk<'a, 'r> {
    arena: &'a Arena,
    run: &'a CrossingRun<'r>,
    session: Session<'a>,
    limit: usize,
}

/// Copies an effect argument out of the run, walked with `ty`, the type
/// its parameter `name` declares.
pub(crate) fn copy_out_tool_arg(
    arena: &Arena,
    run: &CrossingRun<'_>,
    session: Session<'_>,
    limit: usize,
    value: &lash_kernel_doc::Value,
    name: &str,
    ty: &Type,
) -> Result<Datum, CopyError> {
    let walk = Walk {
        arena,
        run,
        session,
        limit,
    };
    let mut at = ArgumentPath {
        steps: vec![name.to_owned()],
    };
    walk.copy(value, &Slot::of(ty), &mut at, limit, &mut Vec::new())
}

type Value = lash_kernel_doc::Value;

impl Walk<'_, '_> {
    fn text(&self, value: &Value) -> Result<String, CopyError> {
        Ok(self.arena.semantics().text(value)?.to_owned())
    }

    fn callable(&self, value: &Value) -> Result<Option<CallableId>, CopyError> {
        Ok(match value {
            Value::Function(handle) => Some(self.arena.callable(*handle)?),
            Value::Closure(handle) => Some(self.arena.closure(*handle)?.0.callable),
            _ => None,
        })
    }

    /// The function `value` crosses as, if it is one: a closure, a
    /// function, or a dialect token whose callable member is a closure
    /// the token's presentation names.
    fn function_root(&self, value: &Value) -> Result<Option<Value>, CopyError> {
        if self.callable(value)?.is_some() {
            return Ok(Some(*value));
        }
        let Value::Tuple(tuple) = value else {
            return Ok(None);
        };
        let members = self.arena.sequence(*tuple)?;
        for (index, member) in members.iter().enumerate() {
            let Value::Closure(handle) = member else {
                continue;
            };
            let identity = self.arena.closure(*handle)?.0;
            let document = defining_document(self.run, identity.callable)?;
            if let Some(start) = lash_kernel_doc::snapshot::closure_at(
                document,
                &identity.site.site,
                self.run.helpers,
            )
            .and_then(|closure| closure.presentation.as_ref())
                && let FunctionRepresentation::Token { tag, callable } = &start.representation
                && *callable as usize == index
                && let Some(first @ (Value::Text(_) | Value::LongText(_))) = members.first()
                && self.text(first)? == *tag
            {
                return Ok(Some(*member));
            }
        }
        Ok(None)
    }

    /// Copies the member one `step` into a value standing at `slot`.
    fn member(
        &self,
        value: &Value,
        slot: &Slot<'_>,
        step: Step<'_>,
        at: &mut ArgumentPath,
        budget: usize,
        path: &mut Vec<ArenaHandle>,
    ) -> Result<Datum, CopyError> {
        // A path names the steps the declared type describes, the step
        // into an open slot among them, and none inside an open value.
        let named = matches!(slot, Slot::Typed(_));
        if named {
            at.steps.push(step_text(&step));
        }
        let copied = self.copy(value, &slot.child(&step), at, budget, path);
        if named {
            at.steps.pop();
        }
        copied
    }

    fn copy(
        &self,
        value: &Value,
        slot: &Slot<'_>,
        at: &mut ArgumentPath,
        budget: usize,
        path: &mut Vec<ArenaHandle>,
    ) -> Result<Datum, CopyError> {
        let Some(budget) = budget.checked_sub(1) else {
            return Err(raised(
                "too_deep",
                format!(
                    "a value copied out of the run exceeds the configured {}-level value depth",
                    self.limit
                ),
            )
            .into());
        };
        if self.function_root(value)?.is_some() {
            if !slot.admits_function() {
                return Err(raised(
                    "not_data",
                    format!(
                        "a function cannot leave the run: `{}` is declared as data, which admits no function",
                        at.render()
                    ),
                )
                .into());
            }
            let function = self.extract(value, budget).map_err(|error| match error {
                CopyError::Raised(mut raised) => {
                    raised.message = format!(
                        "a function cannot leave the run: `{}`: {}",
                        at.render(),
                        raised.message
                    );
                    CopyError::Raised(raised)
                }
                other => other,
            })?;
            return Ok(Datum::FunctionValue(Box::new(function)));
        }
        Ok(match value {
            Value::Tuple(tuple) => {
                let mut items = Vec::new();
                for (index, item) in self.arena.sequence(*tuple)?.iter().enumerate() {
                    items.push(self.member(item, slot, Step::Item(index), at, budget, path)?);
                }
                Datum::Tuple(items)
            }
            Value::List(object)
            | Value::Map(object)
            | Value::Set(object)
            | Value::Record(object) => {
                if path.contains(object) {
                    return Err(
                        raised("cycle", "a value that holds itself cannot leave the run").into(),
                    );
                }
                path.push(*object);
                let copied = match value {
                    Value::List(_) => {
                        let mut items = Vec::new();
                        for (index, item) in self.arena.sequence(*object)?.iter().enumerate() {
                            items.push(self.member(
                                item,
                                slot,
                                Step::Item(index),
                                at,
                                budget,
                                path,
                            )?);
                        }
                        Datum::List(items)
                    }
                    Value::Set(_) => {
                        let mut items = Vec::new();
                        for (index, item) in
                            self.arena.entries(*object)?.iter().step_by(2).enumerate()
                        {
                            items.push(self.member(
                                item,
                                slot,
                                Step::Item(index),
                                at,
                                budget,
                                path,
                            )?);
                        }
                        Datum::Set(items)
                    }
                    Value::Map(_) => {
                        let mut entries = Vec::new();
                        for [key, element] in self.arena.entries(*object)?.as_chunks::<2>().0 {
                            // A text key names its entry, as a field names
                            // its value.
                            let name = match (key, slot) {
                                (Value::Text(_) | Value::LongText(_), Slot::Typed(_)) => {
                                    Some(self.text(key)?)
                                }
                                _ => None,
                            };
                            let copied = self.member(
                                element,
                                slot,
                                Step::Entry(name.as_deref()),
                                at,
                                budget,
                                path,
                            )?;
                            let key = self.member(key, slot, Step::Key, at, budget, path)?;
                            entries.push((key, copied));
                        }
                        Datum::Map(entries)
                    }
                    _ => {
                        let mut fields = Vec::new();
                        for [name, element] in self.arena.entries(*object)?.as_chunks::<2>().0 {
                            let name = self.text(name)?;
                            let copied =
                                self.member(element, slot, Step::Field(&name), at, budget, path)?;
                            fields.push((name, copied));
                        }
                        Datum::Record(fields)
                    }
                };
                path.pop();
                copied
            }
            Value::Error(handle) => match self.arena.body(*handle)? {
                Body::Error(error) => Datum::Error(Box::new(ErrorDatum::new(
                    self.text(&error.kind)?,
                    self.text(&error.message)?,
                    self.member(&error.data, slot, Step::ErrorData, at, budget, path)?,
                ))),
                Body::Cold(_) => {
                    return Err(CopyError::Cold {
                        object: *handle,
                        chunk: 0,
                    });
                }
                _ => {
                    return Err(lash_kernel_doc::NativeError::Malformed(
                        "an error names another body",
                    )
                    .into());
                }
            },
            other => crate::data::copy_out(self.arena, budget + 1, other)?,
        })
    }

    fn extract(&self, value: &Value, budget: usize) -> Result<FunctionValue, CopyError> {
        let mut graph = Graph::default();
        let CaptureValue::Function(function) = graph.function(self, value, budget)? else {
            return Err(raised("not_data", "root callable is not a new environment").into());
        };
        function
            .check(
                self.run.catalog,
                lash_kernel_doc::FunctionValueLimits::standard(),
                self.run.data,
            )
            .map_err(|error| raised("not_data", error.to_string()))?;
        Ok(*function)
    }
}

/// The functions a crossing has frozen so far: a function met again is a
/// reference to its first node, so shared and cyclic captures keep their
/// sharing.
#[derive(Default)]
struct Graph {
    closures: BTreeMap<(u32, u32), u32>,
    functions: BTreeMap<CallableId, u32>,
    count: u32,
}

impl Graph {
    fn function(
        &mut self,
        walk: &Walk<'_, '_>,
        value: &Value,
        budget: usize,
    ) -> Result<CaptureValue, CopyError> {
        let run = walk.run;
        let arena = walk.arena;
        let root = walk
            .function_root(value)?
            .ok_or_else(|| raised("not_data", "no callable root"))?;
        let callable = walk
            .callable(&root)?
            .ok_or_else(|| raised("not_data", "no callable identity"))?;
        let seen = match root {
            Value::Closure(handle) => self.closures.get(&(handle.index(), handle.generation())),
            _ => self.functions.get(&callable),
        };
        if let Some(index) = seen {
            return Ok(CaptureValue::Reference(*index));
        }
        let index = self.count;
        self.count += 1;
        if self.count > FunctionValue::MAX_ENVIRONMENT_NODES {
            return Err(raised("not_data", "callable environment has too many nodes").into());
        }
        match root {
            Value::Closure(handle) => {
                self.closures
                    .insert((handle.index(), handle.generation()), index);
            }
            _ => {
                self.functions.insert(callable, index);
            }
        }
        let document = defining_document(run, callable)?;
        let catalog = LinkedCatalog {
            document,
            helpers: run.helpers,
        };
        let definition = lash_kernel_doc::FunctionCatalog::definition(&catalog, &callable.function)
            .ok_or_else(|| raised("not_data", "callable definition is not loaded"))?;
        let mut signature = definition.signature.clone();
        let mut convention = lash_kernel_doc::StartConvention::Direct;
        let mut representation = FunctionRepresentation::Bare;
        let mut captured = BTreeMap::new();
        let mut creation = None;
        let mut signature_origin = None;
        let mut written_signature = None;
        let mut implicit_text_conversions = Vec::new();
        let mut declared_locals = Vec::new();
        let mut token = None;
        if let Value::Closure(handle) = root {
            let (identity, cells, _) = arena.closure(handle)?;
            let site = &identity.site.site;
            creation = Some(site.clone());
            let origins = run.function_origins;
            if let Some(written) = origins
                .sites
                .get(&(callable.link, site.clone()))
                .or_else(|| origins.functions.get(&callable.function))
                .cloned()
            {
                written_signature = Some(lash_kernel_doc::WrittenSignature {
                    language: written.language,
                    signature: written.signature,
                });
                signature_origin = written.signature_origin;
                implicit_text_conversions = written.implicit_text_conversions;
                declared_locals = written.declared_locals;
            }
            let start = lash_kernel_doc::snapshot::closure_at(document, site, run.helpers)
                .and_then(|closure| closure.presentation.as_ref())
                .ok_or_else(|| raised("not_data", "the function states no start signature"))?;
            signature = start.signature.clone();
            convention = start.convention.clone();
            representation = start.representation.clone();
            if identity.capture_names.len() != cells.len() {
                return Err(lash_kernel_doc::NativeError::Malformed(
                    "a closure's cells are not its capture names",
                )
                .into());
            }
            for (name, cell) in identity.capture_names.iter().zip(cells) {
                captured.insert(name.clone(), arena.cell(*cell)?);
            }
            // A closure `main` made reads the session's bindings by name
            // where it captures none (`K-SES-001`).
            if site.unit == lash_kernel_value::sites::Unit::Main
                && let Some(body) = definition.body()
            {
                let params = definition
                    .signature
                    .params
                    .iter()
                    .map(|param| param.name.clone())
                    .collect::<Vec<_>>();
                for name in
                    lash_kernel_doc::snapshot::CodeReads::of(&params, &body.block, &catalog).free
                {
                    if let std::collections::btree_map::Entry::Vacant(entry) = captured.entry(name)
                        && let Some(value) = (walk.session)(entry.key())
                    {
                        entry.insert(value);
                    }
                }
            }
        } else if let Some(export) = document.exports.values().find_map(|export| match export {
            lash_kernel_doc::Export::Function(export) if export.function == callable.function => {
                Some(export)
            }
            _ => None,
        }) {
            signature = export.signature.clone();
            convention = export.convention.clone();
        }
        if let (Value::Tuple(tuple), FunctionRepresentation::Token { callable: at, .. }) =
            (value, &representation)
        {
            let members = arena.sequence(*tuple)?;
            let mut items = Vec::with_capacity(members.len());
            for (i, member) in members.iter().enumerate() {
                items.push(if i == *at as usize {
                    Datum::Null
                } else {
                    crate::data::copy_out(arena, walk.limit, member)?
                });
            }
            token = Some(items);
        } else {
            representation = FunctionRepresentation::Bare;
        }
        let mut captioned = BTreeSet::new();
        let mut pending = vec![callable.function];
        while let Some(function) = pending.pop() {
            if !captioned.insert(function) {
                continue;
            }
            if let Some(body) = document
                .functions
                .get(&function)
                .and_then(|definition| definition.body())
            {
                pending.extend(lash_kernel_doc::Node::Block(&body.block).functions_within());
            }
        }
        let mut captures = BTreeMap::new();
        for (name, value) in captured {
            captures.insert(name, self.value(walk, &value, budget, &mut Vec::new())?);
        }
        let captions = run
            .captions
            .iter()
            .filter(|entry| {
                entry.target.site().is_some_and(|site| {
                    matches!(&site.unit, lash_kernel_value::sites::Unit::Library(function) if captioned.contains(function))
                })
            })
            .cloned()
            .collect();
        Ok(CaptureValue::Function(Box::new(FunctionValue {
            written_signature,
            signature_origin,
            implicit_text_conversions,
            declared_locals,
            callable,
            captures,
            signature,
            convention,
            representation,
            creation,
            token,
            captions: lash_kernel_doc::Captions::new(document, captions)
                .map_err(|error| raised("not_data", error.to_string()))?,
        })))
    }

    fn value(
        &mut self,
        walk: &Walk<'_, '_>,
        value: &Value,
        budget: usize,
        path: &mut Vec<ArenaHandle>,
    ) -> Result<CaptureValue, CopyError> {
        let arena = walk.arena;
        let budget = budget
            .checked_sub(1)
            .ok_or_else(|| raised("too_deep", "callable captures exceed the value depth bound"))?;
        if walk.function_root(value)?.is_some() {
            return self.function(walk, value, budget);
        }
        Ok(match value {
            Value::Task(_) | Value::TaskRef(_) => {
                return Err(
                    raised("not_data", "the function holds a task or a pending promise").into(),
                );
            }
            Value::Tuple(tuple) => CaptureValue::Tuple(
                arena
                    .sequence(*tuple)?
                    .iter()
                    .map(|member| self.value(walk, member, budget, path))
                    .collect::<Result<_, _>>()?,
            ),
            Value::List(object)
            | Value::Map(object)
            | Value::Set(object)
            | Value::Record(object) => {
                if path.contains(object) {
                    return Err(raised("cycle", "captured data contains itself").into());
                }
                path.push(*object);
                let captured = match value {
                    Value::List(_) => CaptureValue::List(
                        arena
                            .sequence(*object)?
                            .iter()
                            .map(|item| self.value(walk, item, budget, path))
                            .collect::<Result<_, _>>()?,
                    ),
                    Value::Set(_) => CaptureValue::Set(
                        arena
                            .entries(*object)?
                            .iter()
                            .step_by(2)
                            .map(|item| self.value(walk, item, budget, path))
                            .collect::<Result<_, _>>()?,
                    ),
                    Value::Map(_) => {
                        let mut entries = Vec::new();
                        for [key, element] in arena.entries(*object)?.as_chunks::<2>().0 {
                            entries.push((
                                self.value(walk, key, budget, path)?,
                                self.value(walk, element, budget, path)?,
                            ));
                        }
                        CaptureValue::Map(entries)
                    }
                    _ => {
                        let mut fields = Vec::new();
                        for [name, element] in arena.entries(*object)?.as_chunks::<2>().0 {
                            fields
                                .push((walk.text(name)?, self.value(walk, element, budget, path)?));
                        }
                        CaptureValue::Record(fields)
                    }
                };
                path.pop();
                captured
            }
            Value::Error(handle) => match arena.body(*handle)? {
                Body::Error(error) => CaptureValue::Error {
                    kind: walk.text(&error.kind)?,
                    message: walk.text(&error.message)?,
                    data: Box::new(self.value(walk, &error.data, budget, path)?),
                },
                Body::Cold(_) => {
                    return Err(CopyError::Cold {
                        object: *handle,
                        chunk: 0,
                    });
                }
                _ => {
                    return Err(lash_kernel_doc::NativeError::Malformed(
                        "an error names another body",
                    )
                    .into());
                }
            },
            other => CaptureValue::Data(crate::data::copy_out(arena, budget + 1, other)?),
        })
    }
}
