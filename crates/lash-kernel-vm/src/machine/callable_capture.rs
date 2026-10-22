//! Restore frozen callable graphs by allocating every closure cell before filling it.
use super::*;
use lash_kernel_doc::arena::{ClosureIdentity, LocalCode};
use lash_kernel_doc::{CaptureValue, FunctionRepresentation, FunctionValue, NewObject};
use lash_vocabulary::types::Type;

struct Node<'a> {
    function: &'a FunctionValue,
    cells: Vec<(Name, ArenaHandle)>,
    value: Value,
}
impl KernelMachine {
    pub(super) fn restore_callable_environment(
        &mut self,
        exe: &Executable,
        function: &FunctionValue,
    ) -> Eval<Vec<(Name, ArenaHandle)>> {
        let functions = function.nodes();
        let mut nodes = Vec::new();
        for function in &functions {
            let code = match &function.creation {
                Some(creation) => Some(super::project::closure_code(
                    exe,
                    &function.callable.function,
                    creation,
                    &function.captures.keys().cloned().collect::<Vec<_>>(),
                )?),
                None => exe.declared.get(&function.callable.function).copied(),
            };
            let mut cells = Vec::new();
            let closure = if let Some(code) = code {
                let compiled = exe.code(code);
                let names: Vec<_> = compiled
                    .captures
                    .iter()
                    .map(|capture| compiled.slots[capture.inner as usize].name.clone())
                    .chain(compiled.free.iter().map(|(name, _)| name.clone()))
                    .collect();
                if names.len() != function.captures.len()
                    || names
                        .iter()
                        .any(|name| !function.captures.contains_key(name))
                {
                    return Err(
                        fault("callable captures differ from its lexical environment").into(),
                    );
                }
                for name in names {
                    cells.push((name, self.new_cell(Value::Null)));
                }
                let identity = Arc::new(ClosureIdentity {
                    capture_names: compiled.capture_names.clone(),
                    callable: function.callable,
                    site: lash_kernel_doc::linked_site::LinkedSite {
                        link: function.callable.link,
                        site: function
                            .creation
                            .clone()
                            .unwrap_or_else(|| compiled.site.clone()),
                    },
                });
                let closure = self
                    .arena
                    .borrow_mut()
                    .new_closure(identity, cells.iter().map(|(_, cell)| *cell).collect());
                self.arena
                    .borrow_mut()
                    .found_closure_code(
                        closure,
                        LocalCode {
                            executable: exe.local(),
                            code: code.0,
                        },
                    )
                    .map_err(|error| fault(&error.to_string()))?;
                self.grown();
                Value::Closure(closure)
            } else {
                if !function.captures.is_empty() {
                    return Err(fault("native callable has lexical captures").into());
                }
                self.new_object(NewObject::Function(function.callable))?
            };
            let value = match (&function.representation, &function.token) {
                (FunctionRepresentation::Bare, None) => closure,
                (FunctionRepresentation::Token { tag, callable }, Some(token)) => {
                    let mut values = Vec::new();
                    if token.first() != Some(&Datum::Text(tag.clone()))
                        || *callable as usize >= token.len()
                        || token[*callable as usize] != Datum::Null
                    {
                        return Err(fault("invalid captured callable token").into());
                    }
                    for (index, datum) in token.iter().enumerate() {
                        values.push(if index == *callable as usize {
                            closure
                        } else {
                            self.decode(exe, datum, &Type::Any)?
                        });
                    }
                    self.new_object(NewObject::Tuple(values))?
                }
                _ => return Err(fault("callable token differs from its representation").into()),
            };
            nodes.push(Node {
                function,
                cells,
                value,
            });
        }
        // All cells and callable tokens now exist. References can form any cycle.
        for (function, node) in functions.iter().zip(&nodes) {
            for (name, cell) in &node.cells {
                let value = self.restore_capture(exe, &function.captures[name], &nodes)?;
                self.arena
                    .borrow_mut()
                    .cell_set(*cell, value)
                    .map_err(|error| fault(&error.to_string()))?;
            }
        }
        Ok(nodes
            .into_iter()
            .next()
            .ok_or_else(|| fault("callable graph has no root"))?
            .cells)
    }

    fn restore_capture(
        &mut self,
        exe: &Executable,
        capture: &CaptureValue,
        nodes: &[Node<'_>],
    ) -> Eval<Value> {
        Ok(match capture {
            CaptureValue::Data(datum) => self.decode(exe, datum, &Type::Any)?,
            CaptureValue::Reference(index) => {
                nodes
                    .get(*index as usize)
                    .ok_or_else(|| fault("callable reference is outside the graph"))?
                    .value
            }
            CaptureValue::Function(function) => {
                // First-visit nodes retain their address only for this bounded restore.
                // This lookup is identity of the encoded node, never callable identity:
                // equal code may have different captures.
                nodes
                    .iter()
                    .find(|node| std::ptr::eq(node.function, function.as_ref()))
                    .ok_or_else(|| fault("callable node was not preallocated"))?
                    .value
            }
            CaptureValue::Tuple(values)
            | CaptureValue::List(values)
            | CaptureValue::Set(values) => {
                let mut restored = Vec::new();
                for value in values {
                    restored.push(self.restore_capture(exe, value, nodes)?);
                }
                self.new_object(match capture {
                    CaptureValue::Tuple(_) => NewObject::Tuple(restored),
                    CaptureValue::List(_) => NewObject::List(restored),
                    _ => NewObject::Set(restored),
                })?
            }
            CaptureValue::Map(values) => {
                let mut restored = Vec::new();
                for (key, value) in values {
                    restored.push((
                        self.restore_capture(exe, key, nodes)?,
                        self.restore_capture(exe, value, nodes)?,
                    ));
                }
                self.new_object(NewObject::Map(restored))?
            }
            CaptureValue::Record(fields) => {
                let mut restored = Vec::new();
                for (name, value) in fields {
                    let key = self.decode(exe, &Datum::Text(name.clone()), &Type::Text)?;
                    restored.push((key, self.restore_capture(exe, value, nodes)?));
                }
                self.new_object(NewObject::Record(restored))?
            }
            CaptureValue::Error {
                kind,
                message,
                data,
            } => {
                let kind = self.decode(exe, &Datum::Text(kind.clone()), &Type::Text)?;
                let message = self.decode(exe, &Datum::Text(message.clone()), &Type::Text)?;
                let data = self.restore_capture(exe, data, nodes)?;
                self.new_object(NewObject::Error {
                    kind,
                    message,
                    data,
                })?
            }
        })
    }
}
