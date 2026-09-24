use super::*;
use crate::runtime::javascript_array_index_key;
use std::borrow::Cow;

impl Heap {
    fn assignment_index<'a>(
        indexes: &'a [Value],
        index_cursor: &mut usize,
    ) -> Result<&'a Value, RuntimeError> {
        let index = indexes
            .get(*index_cursor)
            .ok_or(RuntimeError::MissingAssignmentIndex)?;
        *index_cursor += 1;
        Ok(index)
    }

    fn javascript_assignment_index_key(
        &self,
        indexes: &[Value],
        index_cursor: usize,
    ) -> Result<Option<String>, RuntimeError> {
        indexes
            .get(index_cursor)
            .map(|index| self.javascript_to_string(index))
            .transpose()
    }

    fn next_javascript_assignment_index_key(
        &self,
        indexes: &[Value],
        index_cursor: &mut usize,
    ) -> Result<String, RuntimeError> {
        let index = Self::assignment_index(indexes, index_cursor)?;
        self.javascript_to_string(index)
    }

    fn assign_regexp_match_index(
        &self,
        result: &mut RegExpMatchObject,
        key: String,
        imported: Value,
        old_logical_bytes: u64,
    ) -> Result<(), RuntimeError> {
        match key.as_str() {
            "index" => result.index = imported,
            "input" => result.input = imported,
            "groups" => result.groups = imported,
            "length" => self.assign_regexp_match_length(result, &imported)?,
            _ => {
                let index = decode_javascript_array_index(key)?;
                if index > result.items.len() {
                    return Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_SPARSE_ARRAY_UNSUPPORTED: assignment index {index} skips array length {}",
                            result.items.len()
                        ),
                    });
                }
                if index == result.items.len() {
                    let attempted = old_logical_bytes.saturating_add(VALUE_SLOT_BYTES + 1);
                    if attempted > self.logical_byte_limit {
                        return Err(RuntimeError::MemoryLimitExceeded {
                            limit: self.logical_byte_limit,
                            attempted,
                        });
                    }
                    result.items.try_reserve_exact(1).map_err(|_| {
                        RuntimeError::MemoryLimitExceeded {
                            limit: self.logical_byte_limit,
                            attempted,
                        }
                    })?;
                    result.items.push(Value::Undefined);
                }
                result.items[index] = imported;
            }
        }
        Ok(())
    }

    fn assign_regexp_match_field(
        &self,
        result: &mut RegExpMatchObject,
        field: &str,
        imported: Value,
    ) -> Result<(), RuntimeError> {
        match field {
            "index" => result.index = imported,
            "input" => result.input = imported,
            "groups" => result.groups = imported,
            "length" => self.assign_regexp_match_length(result, &imported)?,
            _ => {
                return Err(RuntimeError::CannotAssignField {
                    field: field.to_string(),
                    actual: "RegExp match array".to_string(),
                });
            }
        }
        Ok(())
    }

    fn assign_regexp_match_length(
        &self,
        result: &mut RegExpMatchObject,
        imported: &Value,
    ) -> Result<(), RuntimeError> {
        let length = javascript_array_length(self.javascript_to_number(imported)?)?;
        if length > result.items.len() {
            return Err(RuntimeError::ValidationFailed {
                reason: format!(
                    "TS_SPARSE_ARRAY_UNSUPPORTED: assigning RegExp match length {length} would create holes; use indexed appends"
                ),
            });
        }
        result.items.truncate(length);
        Ok(())
    }

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
        let key = self.javascript_to_string(key)?;
        let old_object = self
            .entries
            .get(target_id)
            .ok_or(RuntimeError::DanglingHeapReference {
                id: target_id.get(),
            })?
            .object
            .clone();
        let mut new_object = old_object.clone();
        let deleted = match &mut new_object {
            HeapObject::Record(record) => record.remove(key.as_str()).is_some(),
            HeapObject::List(values) => {
                if key == "length" {
                    return Ok(false);
                }
                if let Some(index) = javascript_array_index_key(key.as_str())
                    && index < values.len()
                {
                    return Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_DELETE_ARRAY_INDEX_UNSUPPORTED: delete on dense array index {index} would create a hole; use splice({index}, 1)"
                        ),
                    });
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
        let entry = self.entry_mut(*target_id)?;
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
            // An image is a host-supplied descriptor, not a heap object: it
            // has no mutable slot at any depth, and saying so by name beats
            // reporting it as an unassignable path.
            if matches!(root, Value::Image(_)) {
                return Err(RuntimeError::ImmutableImageFields);
            }
            // Nothing else has a writable slot. Name the step that was
            // written so the diagnostic points at the program's own syntax
            // rather than at "path".
            let actual = super::super::value_type_name(root).to_string();
            return Err(match path.steps.first() {
                Some(CompiledAssignPathStep::Field(field)) => RuntimeError::CannotAssignField {
                    field: names[*field].text.to_string(),
                    actual,
                },
                Some(CompiledAssignPathStep::Index) => RuntimeError::CannotAssignIndex { actual },
                None => RuntimeError::MissingAssignmentField {
                    field: "path".to_string(),
                },
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
                    let key =
                        self.next_javascript_assignment_index_key(indexes, &mut index_cursor)?;
                    let index = decode_javascript_array_index(key)?;
                    values
                        .get(index)
                        .cloned()
                        .ok_or(RuntimeError::ListAssignmentIndexOutOfBounds)?
                }
                (HeapObject::RegExpMatch(result), CompiledAssignPathStep::Field(field)) => {
                    lookup_regexp_match_field(&result, names[field].text.as_ref())?
                }
                (HeapObject::RegExpMatch(result), CompiledAssignPathStep::Index) => {
                    let key =
                        self.next_javascript_assignment_index_key(indexes, &mut index_cursor)?;
                    lookup_regexp_match_index(&result, &key)?
                }
                (HeapObject::Record(record), CompiledAssignPathStep::Index) => {
                    let key =
                        self.next_javascript_assignment_index_key(indexes, &mut index_cursor)?;
                    record
                        .get(key.as_str())
                        .cloned()
                        .ok_or_else(|| RuntimeError::MissingAssignmentField { field: key })?
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

        let leaf_key: Option<Cow<'_, str>> = match *leaf {
            CompiledAssignPathStep::Field(field) => Some(Cow::Borrowed(names[field].text.as_ref())),
            CompiledAssignPathStep::Index => self
                .javascript_assignment_index_key(indexes, index_cursor)?
                .map(Cow::Owned),
        };
        // `o[key] = v` where `key` only turns out to be `__proto__` here. The
        // value model is dense records with no prototype chain, so the write
        // has nowhere to land except as an ordinary data key — which reads back
        // as data where node's accessor would have changed what the object
        // inherits. The compile-time guard covers every statically named form;
        // this is the one that is only knowable at the write.
        if let Some(error) = leaf_key
            .as_deref()
            .and_then(crate::runtime::access::prototype_chain_key_error)
        {
            return Err(error);
        }
        let is_last_index = leaf_key.as_deref() == Some("lastIndex");
        if is_last_index && matches!(self.get(target_id)?, HeapObject::RegExp(_)) {
            let imported = self.import_values(vec![value], 1)?.remove(0);
            let last_index = regexp_last_index(self.javascript_to_number(&imported)?);
            return self.set_regexp_last_index(target_id, last_index);
        }

        if matches!(self.get(target_id)?, HeapObject::Url(_)) {
            let property = match *leaf {
                CompiledAssignPathStep::Field(field) => names[field].text.to_string(),
                CompiledAssignPathStep::Index => self
                    .javascript_assignment_index_key(indexes, index_cursor)?
                    .ok_or(RuntimeError::MissingAssignmentIndex)?,
            };
            // WHATWG exposes these as getter-only attributes. Assignment in
            // the non-strict script dialect is Node's silent no-op and does
            // not coerce the right-hand side.
            if matches!(property.as_str(), "origin" | "searchParams") {
                return Ok(());
            }
            let value = self.javascript_to_string(&value)?;
            if self.set_url_property(target_id, &property, &value)? {
                return Ok(());
            }
            return Err(RuntimeError::CannotAssignField {
                field: property,
                actual: "URL".to_string(),
            });
        }

        let imported = self.import_values(vec![value], 1)?.remove(0);

        // `xs[xs.length] = item` is an append written as an index write, and
        // it is the other spelling a loop uses to build a list. Taking it
        // through the rebuild below cloned the array twice and re-priced every
        // member, making the loop quadratic; growing the vector in place costs
        // one member. Only the terminal index qualifies — a write inside the
        // array replaces a member and must go on rebuilding so the member it
        // displaces is re-priced and its parent edges retargeted (FIG-3063).
        if matches!(*leaf, CompiledAssignPathStep::Index)
            && let Some(key) = leaf_key.as_deref()
            && let Some(index) = javascript_array_index_key(key)
            && matches!(self.get(target_id)?, HeapObject::List(values) if index == values.len())
        {
            // The rebuild priced a one-slot growth against the object before
            // touching it. Keep that refusal, and keep its wording, so the
            // memory bound answers at the same point it always did.
            let attempted = self
                .object_logical_bytes(target_id)?
                .saturating_add(VALUE_SLOT_BYTES + 1);
            if attempted > self.logical_byte_limit {
                return Err(RuntimeError::MemoryLimitExceeded {
                    limit: self.logical_byte_limit,
                    attempted,
                });
            }
            self.append_javascript_list(target_id, std::slice::from_ref(&imported))?;
            return Ok(());
        }

        let old_object = self
            .entries
            .get(&target_id)
            .ok_or(RuntimeError::DanglingHeapReference {
                id: target_id.get(),
            })?
            .object
            .clone();
        let mut new_object = old_object.clone();
        let mut filled_hole_index = None;
        match (&mut new_object, *leaf) {
            (HeapObject::Record(record), CompiledAssignPathStep::Field(field)) => {
                record.insert_symbolized(names[field].symbol, names[field].text.clone(), imported);
            }
            (HeapObject::List(values), CompiledAssignPathStep::Field(field))
                if names[field].text.as_ref() == "length" =>
            {
                let length = crate::runtime::javascript_to_number(&imported);
                if !length.is_finite()
                    || length < 0.0
                    || length.fract() != 0.0
                    || length > u32::MAX as f64
                {
                    return Err(RuntimeError::ValidationFailed {
                        reason: "RangeError: Invalid array length".to_string(),
                    });
                }
                let length = length as usize;
                if length > values.len() {
                    return Err(RuntimeError::ValidationFailed {
                        reason: format!(
                            "TS_SPARSE_ARRAY_UNSUPPORTED: growing array length from {} to {length} would create holes; append values explicitly",
                            values.len()
                        ),
                    });
                }
                values.truncate(length);
            }
            (HeapObject::List(values), CompiledAssignPathStep::Index) => {
                let key = self
                    .javascript_assignment_index_key(indexes, index_cursor)?
                    .ok_or(RuntimeError::MissingAssignmentIndex)?;
                let index = decode_javascript_array_index(key)?;
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
                filled_hole_index = Some(index);
            }
            (HeapObject::RegExpMatch(result), CompiledAssignPathStep::Index) => {
                let key = self
                    .javascript_assignment_index_key(indexes, index_cursor)?
                    .ok_or(RuntimeError::MissingAssignmentIndex)?;
                self.assign_regexp_match_index(result, key, imported, old_object.logical_bytes())?;
            }
            (HeapObject::RegExpMatch(result), CompiledAssignPathStep::Field(field)) => {
                self.assign_regexp_match_field(result, names[field].text.as_ref(), imported)?;
            }
            (HeapObject::Record(record), CompiledAssignPathStep::Index) => {
                let key = leaf_key
                    .as_deref()
                    .ok_or(RuntimeError::MissingAssignmentIndex)?;
                record.insert_str(key, imported);
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
        let committed = self.commit_reference_assignment(target_id, old_object, new_object);
        if committed.is_ok()
            && let Some(index) = filled_hole_index
        {
            self.clear_list_hole(target_id, index);
        }
        committed
    }

    fn commit_reference_assignment(
        &mut self,
        target_id: HeapId,
        old_object: HeapObject,
        new_object: HeapObject,
    ) -> Result<(), RuntimeError> {
        let new_bytes = new_object.logical_bytes();
        let next_live = self
            .live_logical_bytes
            .saturating_sub(old_object.logical_bytes())
            .saturating_add(new_bytes);
        if next_live > self.logical_byte_limit {
            return Err(RuntimeError::MemoryLimitExceeded {
                limit: self.logical_byte_limit,
                attempted: next_live,
            });
        }
        let old_children = old_object.child_refs();
        let new_children = new_object.child_refs();
        let entry = self.entry_mut(target_id)?;
        entry.object = new_object;
        entry.logical_bytes = new_bytes;
        self.live_logical_bytes = next_live;
        self.retarget_parent_edges(target_id, &old_children, &new_children);
        self.invalidate_materialized_reaching(target_id);
        self.debug_assert_byte_accounting();
        Ok(())
    }
}

fn decode_javascript_array_index(key: String) -> Result<usize, RuntimeError> {
    javascript_array_index_key(&key)
        .ok_or(RuntimeError::TypeScriptArrayNonIndexPropertyUnsupported { key })
}

fn lookup_regexp_match_field(
    result: &RegExpMatchObject,
    field: &str,
) -> Result<Value, RuntimeError> {
    match field {
        "index" => Ok(result.index.clone()),
        "input" => Ok(result.input.clone()),
        "groups" => Ok(result.groups.clone()),
        _ => Err(RuntimeError::MissingAssignmentField {
            field: field.to_string(),
        }),
    }
}

fn lookup_regexp_match_index(result: &RegExpMatchObject, key: &str) -> Result<Value, RuntimeError> {
    match key {
        "index" => Ok(result.index.clone()),
        "input" => Ok(result.input.clone()),
        "groups" => Ok(result.groups.clone()),
        _ => javascript_array_index_key(key)
            .and_then(|index| result.items.get(index).cloned())
            .ok_or(RuntimeError::ListAssignmentIndexOutOfBounds),
    }
}

fn javascript_array_length(number: f64) -> Result<usize, RuntimeError> {
    if number.is_finite() && number >= 0.0 && number.fract() == 0.0 && number <= u32::MAX as f64 {
        return Ok(number as usize);
    }
    Err(RuntimeError::ValidationFailed {
        reason: "RangeError: Invalid array length".to_string(),
    })
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
