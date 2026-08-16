use super::*;
use crate::runtime::javascript_array_index_key;

impl Heap {
    pub(crate) fn delete_javascript_member(
        &mut self,
        receiver: &Value,
        key: &Value,
    ) -> Result<bool, RuntimeError> {
        let Value::Ref(target_id) = receiver else {
            return match receiver {
                Value::Null | Value::Undefined => Err(RuntimeError::ValidationFailed {
                    reason: "TypeError: Cannot convert undefined or null to object".to_string(),
                }),
                _ => Ok(true),
            };
        };
        let key = coerce_string(key)?;
        let slot =
            self.id_to_slot
                .get(target_id)
                .copied()
                .ok_or(RuntimeError::DanglingHeapReference {
                    id: target_id.get(),
                })?;
        let old_object = self.slots[slot]
            .as_ref()
            .ok_or(RuntimeError::DanglingHeapReference {
                id: target_id.get(),
            })?
            .object
            .clone();
        let mut new_object = old_object.clone();
        let deleted = match &mut new_object {
            HeapObject::Record(record) => record.remove(key.as_ref()).is_some(),
            HeapObject::List(values) => {
                if key.as_ref() == "length" {
                    return Ok(false);
                }
                if let Some(index) = javascript_array_index(&Value::String(key.as_ref().into())) {
                    if index < values.len() {
                        return Err(RuntimeError::ValidationFailed {
                            reason: format!(
                                "TS_DELETE_ARRAY_INDEX_UNSUPPORTED: delete on dense array index {index} would create a hole; use splice({index}, 1)"
                            ),
                        });
                    }
                }
                false
            }
            HeapObject::Tuple(_) => return Ok(false),
            object => {
                return Err(RuntimeError::ValidationFailed {
                    reason: format!(
                        "TS_DELETE_EXOTIC_MEMBER_UNSUPPORTED: delete on {} members is unsupported",
                        object.kind_name()
                    ),
                });
            }
        };
        if !deleted {
            return Ok(true);
        }
        let old_bytes = old_object.logical_bytes();
        let new_bytes = new_object.logical_bytes();
        let old_children = old_object.child_refs();
        let new_children = new_object.child_refs();
        let entry = self.slots[slot].as_mut().expect("heap slot exists");
        entry.object = new_object;
        entry.logical_bytes = new_bytes;
        self.live_logical_bytes = self
            .live_logical_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        self.retarget_parent_edges(*target_id, &old_children, &new_children);
        self.invalidate_materialized_reaching(*target_id);
        self.debug_assert_byte_accounting();
        Ok(true)
    }

    pub(crate) fn assign_path_reference(
        &mut self,
        root: &Value,
        path: &CompiledAssignPath,
        indexes: &[Value],
        value: Value,
        names: &[Name],
    ) -> Result<(), RuntimeError> {
        let &Value::Ref(mut target_id) = root else {
            return Err(RuntimeError::CannotAssignField {
                field: "path".to_string(),
                actual: super::super::value_type_name(root).to_string(),
            });
        };
        let Some((leaf, parents)) = path.steps.split_last() else {
            return Err(RuntimeError::MissingAssignmentField {
                field: "path".to_string(),
            });
        };
        let mut index_cursor = 0;
        for step in parents {
            let child = match (self.get(target_id)?.clone(), *step) {
                (HeapObject::Record(record), CompiledAssignPathStep::Field(field)) => record
                    .get_symbol(names[field].symbol)
                    .cloned()
                    .ok_or_else(|| RuntimeError::MissingAssignmentField {
                        field: names[field].text.to_string(),
                    })?,
                (HeapObject::List(values), CompiledAssignPathStep::Index) => {
                    let index = indexes
                        .get(index_cursor)
                        .ok_or(RuntimeError::MissingAssignmentIndex)?;
                    index_cursor += 1;
                    let key = self.javascript_to_string(index)?;
                    let index = javascript_array_index_key(&key).ok_or_else(|| {
                        RuntimeError::TypeScriptArrayNonIndexPropertyUnsupported { key }
                    })?;
                    values
                        .get(index)
                        .cloned()
                        .ok_or(RuntimeError::ListAssignmentIndexOutOfBounds)?
                }
                (HeapObject::Record(record), CompiledAssignPathStep::Index) => {
                    let index = indexes
                        .get(index_cursor)
                        .ok_or(RuntimeError::MissingAssignmentIndex)?;
                    index_cursor += 1;
                    let key = coerce_string(index)?;
                    record.get(key.as_ref()).cloned().ok_or_else(|| {
                        RuntimeError::MissingAssignmentField {
                            field: key.into_owned(),
                        }
                    })?
                }
                (object, CompiledAssignPathStep::Field(field)) => {
                    return Err(RuntimeError::CannotAssignField {
                        field: names[field].text.to_string(),
                        actual: object.kind_name().to_string(),
                    });
                }
                (object, CompiledAssignPathStep::Index) => {
                    return Err(RuntimeError::CannotAssignIndex {
                        actual: object.kind_name().to_string(),
                    });
                }
            };
            let Value::Ref(child_id) = child else {
                return Err(RuntimeError::CannotAssignField {
                    field: "nested path".to_string(),
                    actual: super::super::value_type_name(&child).to_string(),
                });
            };
            target_id = child_id;
        }

        let is_last_index = match *leaf {
            CompiledAssignPathStep::Field(field) => names[field].text.as_ref() == "lastIndex",
            CompiledAssignPathStep::Index => indexes
                .get(index_cursor)
                .map(|index| self.javascript_to_string(index))
                .transpose()?
                .is_some_and(|key| key == "lastIndex"),
        };
        if is_last_index && matches!(self.get(target_id)?, HeapObject::RegExp(_)) {
            let imported = self.import_values(vec![value], 1)?.remove(0);
            let last_index = regexp_last_index(self.javascript_to_number(&imported)?);
            return self.set_regexp_last_index(target_id, last_index);
        }

        let imported = self.import_values(vec![value], 1)?.remove(0);
        let slot = self.id_to_slot.get(&target_id).copied().ok_or(
            RuntimeError::DanglingHeapReference {
                id: target_id.get(),
            },
        )?;
        let old_object = self.slots[slot]
            .as_ref()
            .ok_or(RuntimeError::DanglingHeapReference {
                id: target_id.get(),
            })?
            .object
            .clone();
        let mut new_object = old_object.clone();
        match (&mut new_object, *leaf) {
            (HeapObject::Record(record), CompiledAssignPathStep::Field(field)) => {
                record.insert_symbolized(names[field].symbol, names[field].text.clone(), imported);
            }
            (HeapObject::List(values), CompiledAssignPathStep::Index) => {
                let index = indexes
                    .get(index_cursor)
                    .ok_or(RuntimeError::MissingAssignmentIndex)?;
                let key = self.javascript_to_string(index)?;
                let index = javascript_array_index_key(&key).ok_or_else(|| {
                    RuntimeError::TypeScriptArrayNonIndexPropertyUnsupported { key }
                })?;
                if index > values.len() {
                    return Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_SPARSE_ARRAY_UNSUPPORTED: assignment index {index} skips array length {}",
                            values.len()
                        ),
                    });
                }
                if index == values.len() {
                    let added = index + 1 - values.len();
                    let attempted = old_object
                        .logical_bytes()
                        .saturating_add((added as u64).saturating_mul(VALUE_SLOT_BYTES + 1));
                    if attempted > self.logical_byte_limit {
                        return Err(RuntimeError::MemoryLimitExceeded {
                            limit: self.logical_byte_limit,
                            attempted,
                        });
                    }
                    values.resize(index + 1, Value::Undefined);
                }
                values[index] = imported;
            }
            (HeapObject::Record(record), CompiledAssignPathStep::Index) => {
                let index = indexes
                    .get(index_cursor)
                    .ok_or(RuntimeError::MissingAssignmentIndex)?;
                let key = coerce_string(index)?;
                record.insert_str(key.as_ref(), imported);
            }
            (HeapObject::Tuple(_), CompiledAssignPathStep::Index) => {
                return Err(RuntimeError::ImmutableTupleIndexes);
            }
            (object, CompiledAssignPathStep::Field(field)) => {
                return Err(RuntimeError::CannotAssignField {
                    field: names[field].text.to_string(),
                    actual: object.kind_name().to_string(),
                });
            }
            (object, CompiledAssignPathStep::Index) => {
                return Err(RuntimeError::CannotAssignIndex {
                    actual: object.kind_name().to_string(),
                });
            }
        }
        let old_bytes = old_object.logical_bytes();
        let new_bytes = new_object.logical_bytes();
        let next_live = self
            .live_logical_bytes
            .saturating_sub(old_bytes)
            .saturating_add(new_bytes);
        if next_live > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted: next_live,
            });
        }
        let old_children = old_object.child_refs();
        let new_children = new_object.child_refs();
        let entry = self.slots[slot].as_mut().expect("heap slot exists");
        entry.object = new_object;
        entry.logical_bytes = new_bytes;
        self.live_logical_bytes = next_live;
        self.retarget_parent_edges(target_id, &old_children, &new_children);
        self.invalidate_materialized_reaching(target_id);
        self.debug_assert_byte_accounting();
        Ok(())
    }
}

fn regexp_last_index(number: f64) -> u64 {
    if number.is_nan() || number <= 0.0 {
        return 0;
    }
    if number.is_infinite() || number >= MAX_JAVASCRIPT_LENGTH as f64 {
        return MAX_JAVASCRIPT_LENGTH;
    }
    number.trunc() as u64
}
