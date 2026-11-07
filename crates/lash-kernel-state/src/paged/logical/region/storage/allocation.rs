use super::*;

impl StateLease<'_> {
    /// Derive the canonical constructor rows without copying operand values or
    /// reserving their target body. Scratch grows only with distinct keys and
    /// is reused by subsequent phases, including abandoned candidates.
    pub(super) fn normalize_table_inputs(
        &mut self,
        body: &Allocation<'_>,
    ) -> Result<(), LeaseFault> {
        let len = match body {
            Allocation::Map(entries) | Allocation::Record(entries) => entries.len(),
            Allocation::Set(values) => values.len(),
            Allocation::RecordFields { fields, values } => {
                if fields.len() != values.len() {
                    return Err(invalid("record fields differ from their values").into());
                }
                fields.len()
            }
            _ => unreachable!("table allocation descriptor"),
        };
        for at in 0..len {
            let normalized = match body {
                Allocation::RecordFields { fields, values } => {
                    self.local.value_index(values[at])?;
                    LeaseKey::Canonical(Key::of_text(&fields[at]))
                }
                _ => {
                    let key = match body {
                        Allocation::Map(entries) | Allocation::Record(entries) => {
                            self.local.value_index(entries[at].1)?;
                            entries[at].0
                        }
                        Allocation::Set(values) => values[at],
                        _ => unreachable!("table allocation descriptor"),
                    };
                    if matches!(body, Allocation::Record(_)) && !self.is_text(key)? {
                        return Err(NativeError::Decline.into());
                    }
                    self.key(key)?
                }
            };
            let inputs = &mut self.local.table_inputs;
            if let Some(row) = inputs.index.get(&normalized).copied() {
                inputs.rows[row].1 = at;
            } else {
                capacity(&mut inputs.rows, 1)?;
                inputs.index.try_reserve(1).map_err(|_| exhausted())?;
                inputs.index.insert(normalized, inputs.rows.len());
                inputs.rows.push((at, at));
            }
        }
        Ok(())
    }
    pub(super) fn normalized_entry(&self, body: &Allocation<'_>, row: usize) -> Entry {
        let (first, last) = self.local.table_inputs.rows[row];
        if let Allocation::RecordFields { fields, values } = body {
            return Entry {
                key: Spelling::Field(Arc::clone(&fields[first])),
                value: values[last],
                sequence: row as u64,
            };
        }
        let (key, value) = match body {
            Allocation::Map(entries) | Allocation::Record(entries) => {
                (entries[first].0, entries[last].1)
            }
            Allocation::Set(values) => (
                values[first],
                LeaseValue {
                    repr: Repr::Scalar(Value::Absent),
                },
            ),
            _ => unreachable!("table allocation descriptor"),
        };
        Entry {
            key: Spelling::Value(key),
            value,
            sequence: row as u64,
        }
    }
    pub(super) fn reserve_allocation(
        &self,
        body: &Allocation<'_>,
        mut reusable: Option<LocalBody>,
    ) -> Result<LocalBody, HeapFault> {
        pub(super) fn values(
            source: &[LeaseValue],
            reusable: Option<LocalBody>,
        ) -> Result<Vec<LeaseValue>, HeapFault> {
            let mut values = match reusable {
                Some(LocalBody::Values(values)) => values,
                _ => Vec::new(),
            };
            values.clear();
            capacity(&mut values, source.len())?;
            values.extend_from_slice(source);
            Ok(values)
        }
        Ok(match body {
            Allocation::List(source) | Allocation::Tuple(source) => {
                LocalBody::Values(values(source, reusable.take())?)
            }
            Allocation::Cell(value) => LocalBody::Cell(**value),
            Allocation::Closure { identity, captures } => {
                let mut values = match reusable.take() {
                    Some(LocalBody::Closure(closure)) => closure.captures,
                    _ => Vec::new(),
                };
                values.clear();
                capacity(&mut values, captures.len())?;
                values.extend_from_slice(captures);
                LocalBody::Closure(LocalClosure {
                    identity: Arc::clone(identity),
                    captures: values,
                })
            }
            Allocation::Text(text) => {
                let mut value = match reusable.take() {
                    Some(LocalBody::Text(value)) => value,
                    _ => String::new(),
                };
                value.clear();
                value.try_reserve(text.len()).map_err(|_| exhausted())?;
                value.push_str(text);
                LocalBody::Text(value)
            }
            Allocation::Bytes(source) => {
                let mut value = match reusable.take() {
                    Some(LocalBody::Bytes(value)) => value,
                    _ => Vec::new(),
                };
                value.clear();
                capacity(&mut value, source.len())?;
                value.extend_from_slice(source);
                LocalBody::Bytes(value)
            }
            Allocation::BigInt(integer) => LocalBody::Leaf(LeafBody::Integer((*integer).clone())),
            Allocation::Function(callable) => LocalBody::Leaf(LeafBody::Function(*callable)),
            Allocation::Map(_)
            | Allocation::Record(_)
            | Allocation::RecordFields { .. }
            | Allocation::Set(_) => {
                let (mut entries, mut index) = match reusable.take() {
                    Some(LocalBody::Table(table)) => (table.entries, table.index),
                    _ => (Vec::new(), std::collections::HashMap::default()),
                };
                entries.clear();
                index.clear();
                let inputs = &self.local.table_inputs;
                capacity(&mut entries, inputs.rows.len())?;
                index
                    .try_reserve(inputs.rows.len())
                    .map_err(|_| exhausted())?;
                for row in 0..inputs.rows.len() {
                    entries.push(self.normalized_entry(body, row));
                }
                for (key, row) in &inputs.index {
                    index.insert(key.clone(), *row);
                }
                LocalBody::Table(LocalTable {
                    entries,
                    index,
                    next: inputs.rows.len() as u64,
                })
            }
        })
    }
}
