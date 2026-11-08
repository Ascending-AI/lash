//! Member reads and writes (`K-FORM-009`, `K-EVAL-003`, `K-VAL`): what
//! `Index`, `Field`, the `Set*` and the `Remove*` ops do to the value on
//! the stack, in place in the heap.

use lash_kernel_doc::Value;
use lash_kernel_doc::arena::Body;

use super::{Run, Vm};
use crate::heap::Heap;

/// The list position `index` names, `None` past every position.
fn position<H: Heap>(vm: &mut Vm<H>, index: &Value) -> Run<Option<u64>> {
    match index {
        Value::Int(word) => Ok(u64::try_from(*word).ok()),
        Value::BigInt(_) => Ok(None),
        Value::Float(float) => {
            let float = float.get();
            Ok(
                (float.fract() == 0.0 && float >= 0.0 && float < u64::MAX as f64)
                    .then_some(float as u64),
            )
        }
        _ => Err(vm.error("type_error", "a list is indexed by a number")),
    }
}

fn is_text(value: &Value) -> bool {
    matches!(value, Value::Text(_) | Value::LongText(_))
}

impl<H: Heap> Vm<H> {
    pub(super) fn index(&mut self, target: Value, index: Value) -> Run<Value> {
        match target {
            Value::Tuple(object) | Value::List(object) => {
                let at = position(self, &index)?;
                let arena = self.heap.arena();
                let element = match at {
                    Some(at) => arena.item(object, at).map_err(|e| self.native(e))?,
                    None => None,
                };
                match element {
                    Some(value) => Ok(value),
                    None => {
                        let len = self.heap.arena().len(object).map_err(|e| self.native(e))?;
                        Err(self.error(
                            "index_out_of_range",
                            &format!("no element there; the length is {len}"),
                        ))
                    }
                }
            }
            Value::Record(record) => {
                if !is_text(&index) {
                    return Err(self.error("type_error", "a record index must be text"));
                }
                let found = self
                    .heap
                    .arena()
                    .table_get(record, &index)
                    .map_err(|e| self.native(e))?;
                Ok(found.unwrap_or(Value::Absent))
            }
            Value::Map(map) => {
                let found = self
                    .heap
                    .arena()
                    .table_get(map, &index)
                    .map_err(|e| self.native(e))?;
                found
                    .ok_or_else(|| self.error("key_missing", "the map has no entry under that key"))
            }
            Value::Set(set) => {
                let found = self
                    .heap
                    .arena()
                    .table_get(set, &index)
                    .map_err(|e| self.native(e))?;
                Ok(Value::Bool(found.is_some()))
            }
            _ => Err(self.error(
                "type_error",
                "only a list, a tuple, a record, a map or a set is read by index",
            )),
        }
    }

    pub(super) fn field(&mut self, target: Value, name: Value) -> Run<Value> {
        match target {
            Value::Record(record) => {
                let found = self
                    .heap
                    .arena()
                    .table_get(record, &name)
                    .map_err(|e| self.native(e))?;
                Ok(found.unwrap_or(Value::Absent))
            }
            Value::Error(error) => {
                let parts = match self.heap.arena().body(error) {
                    Ok(Body::Error(parts)) => *parts,
                    Ok(_) => return Err(super::fault("an error handle names another body")),
                    Err(e) => return Err(self.native(e)),
                };
                let text = self.text_of(&name)?;
                Ok(match text.as_str() {
                    "kind" => parts.kind,
                    "message" => parts.message,
                    "object" => parts.data,
                    "data" => match parts.data {
                        Value::Record(record) => {
                            let raw = self
                                .heap
                                .arena_mut()
                                .text(lash_kernel_doc::error_object::RAW);
                            self.heap
                                .arena()
                                .table_get(record, &raw)
                                .map_err(|e| self.native(e))?
                                .unwrap_or(Value::Absent)
                        }
                        _ => Value::Absent,
                    },
                    _ => Value::Absent,
                })
            }
            _ => Err(self.error("type_error", "only a record or an error has fields")),
        }
    }

    /// The text a field-name constant spells.
    fn text_of(&mut self, name: &Value) -> Run<String> {
        let text = self.heap.semantics().text(name).map(str::to_owned);
        text.map_err(|e| self.native(e))
    }

    pub(super) fn set_field(&mut self, target: Value, name: Value, value: Value) -> Run<()> {
        let Value::Record(record) = target else {
            return Err(self.error("type_error", "only a record has fields to assign"));
        };
        self.heap
            .arena_mut()
            .table_write(record, name, value)
            .map_err(|e| self.native(e))
    }

    pub(super) fn set_index(&mut self, target: Value, index: Value, value: Value) -> Run<()> {
        let written = match target {
            Value::List(list) => {
                let length = self.heap.arena().len(list).map_err(|e| self.native(e))?;
                let Some(at) = position(self, &index)?.filter(|at| *at <= length) else {
                    return Err(self.error(
                        "index_out_of_range",
                        &format!("a list of {length} is assigned at 0 to {length}"),
                    ));
                };
                self.heap.arena_mut().list_set(list, at, value)
            }
            Value::Record(record) => {
                if !is_text(&index) {
                    return Err(self.error("type_error", "a record index must be text"));
                }
                self.heap.arena_mut().table_write(record, index, value)
            }
            Value::Map(map) => {
                self.key(&index)?;
                self.heap.arena_mut().table_write(map, index, value)
            }
            Value::Set(set) => {
                self.key(&index)?;
                let Value::Bool(present) = value else {
                    return Err(self.error(
                        "type_error",
                        "a set's member is assigned `true` to add it or `false` to remove it",
                    ));
                };
                if present {
                    self.heap.arena_mut().table_write(set, index, Value::Absent)
                } else {
                    self.heap.arena_mut().table_delete(set, &index)
                }
            }
            _ => {
                return Err(self.error(
                    "type_error",
                    "only a list, a record, a map or a set is assigned by index",
                ));
            }
        };
        written.map_err(|e| self.native(e))
    }

    pub(super) fn remove_field(&mut self, target: Value, name: Value) -> Run<()> {
        let Value::Record(record) = target else {
            return Err(self.error("type_error", "only a record has fields to remove"));
        };
        self.heap
            .arena_mut()
            .table_delete(record, &name)
            .map_err(|e| self.native(e))
    }

    pub(super) fn remove_index(&mut self, target: Value, index: Value) -> Run<()> {
        let removed = match target {
            Value::List(list) => {
                let length = self.heap.arena().len(list).map_err(|e| self.native(e))?;
                let Some(at) = position(self, &index)?.filter(|at| *at < length) else {
                    return Err(self.error(
                        "index_out_of_range",
                        &format!("a list of {length} has no element there to remove"),
                    ));
                };
                self.heap.arena_mut().list_remove(list, at)
            }
            Value::Record(record) => {
                if !is_text(&index) {
                    return Err(self.error("type_error", "a record index must be text"));
                }
                self.heap.arena_mut().table_delete(record, &index)
            }
            Value::Map(table) | Value::Set(table) => {
                self.key(&index)?;
                self.heap.arena_mut().table_delete(table, &index)
            }
            _ => {
                return Err(self.error(
                    "type_error",
                    "only a list, a record, a map or a set has an index to remove",
                ));
            }
        };
        removed.map_err(|e| self.native(e))
    }
}
