use super::*;

impl StateLease<'_> {
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
            Allocation::Map(source) | Allocation::Record(source) => {
                let (mut entries, mut index) = match reusable.take() {
                    Some(LocalBody::Table(table)) => (table.entries, table.index),
                    _ => (Vec::new(), std::collections::HashMap::default()),
                };
                entries.clear();
                index.clear();
                capacity(&mut entries, source.len())?;
                index.try_reserve(source.len()).map_err(|_| exhausted())?;
                for (at, (key, value)) in source.iter().enumerate() {
                    let normalized = self
                        .key(*key)
                        .map_err(|error| invalid(&error.to_string()))?;
                    if index.insert(normalized, at).is_some() {
                        return Err(invalid(
                            "map allocation repeats a member; lower its ordered member phases",
                        ));
                    }
                    entries.push(Entry {
                        key: Spelling::Value(*key),
                        value: *value,
                        sequence: at as u64,
                    });
                }
                LocalBody::Table(LocalTable {
                    entries,
                    index,
                    next: source.len() as u64,
                })
            }
            Allocation::Set(source) => {
                let (mut entries, mut index) = match reusable.take() {
                    Some(LocalBody::Table(table)) => (table.entries, table.index),
                    _ => (Vec::new(), std::collections::HashMap::default()),
                };
                entries.clear();
                index.clear();
                capacity(&mut entries, source.len())?;
                index.try_reserve(source.len()).map_err(|_| exhausted())?;
                for (at, key) in source.iter().enumerate() {
                    let normalized = self
                        .key(*key)
                        .map_err(|error| invalid(&error.to_string()))?;
                    if index.insert(normalized, at).is_some() {
                        return Err(invalid("set allocation repeats a member"));
                    }
                    entries.push(Entry {
                        key: Spelling::Value(*key),
                        value: LeaseValue {
                            repr: Repr::Scalar(Value::Absent),
                        },
                        sequence: at as u64,
                    });
                }
                LocalBody::Table(LocalTable {
                    entries,
                    index,
                    next: source.len() as u64,
                })
            }
        })
    }
}
