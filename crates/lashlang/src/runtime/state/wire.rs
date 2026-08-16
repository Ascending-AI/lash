use super::*;
use crate::runtime::RegExpMatchObject;

impl CanonicalHeapObject {
    pub(super) fn from_runtime(object: &HeapObject, id: HeapId) -> Result<Self, ContinuationError> {
        let location = format!("heap.objects[{}]", id.get());
        Ok(match object {
            HeapObject::Tuple(values) => Self::Tuple {
                items: canonical_items(values, &location, 0)?,
            },
            HeapObject::List(values) => Self::List {
                items: canonical_items(values, &location, 0)?,
            },
            HeapObject::Record(record) => {
                let mut fields = record.iter().collect::<Vec<_>>();
                fields.sort_unstable_by_key(|(name, _)| *name);
                Self::Record {
                    fields: fields
                        .into_iter()
                        .map(|(name, value)| {
                            Ok(CanonicalBinding {
                                name: name.to_string(),
                                value: CanonicalValue::from_runtime(
                                    value,
                                    &child_location(&location, name),
                                    0,
                                )?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                }
            }
            HeapObject::Closure { function, captures } => Self::Closure {
                function: *function,
                captures: canonical_items(captures, &location, 0)?,
            },
            HeapObject::RegExp(regexp) => Self::RegExp {
                pattern: regexp.pattern.clone(),
                flags: regexp.flags.clone(),
                last_index: regexp.last_index,
            },
            HeapObject::RegExpMatch(result) => Self::RegExpMatch {
                items: canonical_items(&result.items, &location, 0)?,
                index: CanonicalValue::from_runtime(
                    &result.index,
                    &format!("{location}.index"),
                    0,
                )?,
                input: CanonicalValue::from_runtime(
                    &result.input,
                    &format!("{location}.input"),
                    0,
                )?,
                groups: CanonicalValue::from_runtime(
                    &result.groups,
                    &format!("{location}.groups"),
                    0,
                )?,
            },
            HeapObject::Map(map) => Self::Map {
                entries: map
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(index, (key, value))| {
                        let location = format!("{location}.entries[{index}]");
                        Ok(CanonicalMapEntry {
                            key: CanonicalValue::from_runtime(key, &format!("{location}.key"), 0)?,
                            value: CanonicalValue::from_runtime(
                                value,
                                &format!("{location}.value"),
                                0,
                            )?,
                        })
                    })
                    .collect::<Result<_, ContinuationError>>()?,
            },
            HeapObject::Set(set) => Self::Set {
                values: canonical_items(&set.values, &location, 0)?,
            },
            HeapObject::Date(date) => Self::Date {
                milliseconds: normalize_number(date.milliseconds),
            },
            HeapObject::Error(error) => Self::Error {
                error_kind: error.kind,
                message: error.message.clone(),
                cause: error
                    .cause
                    .as_ref()
                    .map(|value| {
                        CanonicalValue::from_runtime(value, &format!("{location}.cause"), 0)
                    })
                    .transpose()?,
                errors: error
                    .errors
                    .as_ref()
                    .map(|value| {
                        CanonicalValue::from_runtime(value, &format!("{location}.errors"), 0)
                    })
                    .transpose()?,
            },
            HeapObject::Url(url) => Self::Url {
                href: url.href.clone(),
                search_params: CanonicalValue::from_runtime(
                    &url.search_params,
                    &format!("{location}.search_params"),
                    0,
                )?,
            },
            HeapObject::UrlSearchParams(params) => Self::UrlSearchParams {
                entries: params
                    .entries
                    .iter()
                    .map(|(key, value)| CanonicalUrlSearchParamsEntry {
                        key: key.clone(),
                        value: value.clone(),
                    })
                    .collect(),
            },
        })
    }

    pub(super) fn into_runtime(self) -> Result<HeapObject, SnapshotDecodeError> {
        Ok(match self {
            Self::Tuple { items } => HeapObject::Tuple(
                items
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
            ),
            Self::List { items } => HeapObject::List(
                items
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Record { fields } => HeapObject::Record(Box::new(
                fields
                    .into_iter()
                    .map(|field| {
                        ensure_no_prototype_chain_wire_key(&field.name)?;
                        field.value.into_runtime().map(|value| (field.name, value))
                    })
                    .collect::<Result<_, _>>()?,
            )),
            Self::Closure { function, captures } => HeapObject::Closure {
                function,
                captures: captures
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
            },
            Self::RegExp {
                pattern,
                flags,
                last_index,
            } => {
                crate::runtime::validate_typescript_regexp(&pattern, &flags).map_err(|error| {
                    SnapshotDecodeError::InvalidEncoding(format!(
                        "RegExp pattern or flags violate TypeScript bounds: {}",
                        error.diagnostic_code()
                    ))
                })?;
                if last_index > crate::runtime::heap::MAX_JAVASCRIPT_LENGTH {
                    return Err(SnapshotDecodeError::InvalidEncoding(
                        "RegExp last_index exceeds JavaScript's maximum safe length".to_string(),
                    ));
                }
                HeapObject::RegExp(RegExpObject {
                    pattern,
                    flags,
                    last_index,
                    compiled_program: None,
                })
            }
            Self::RegExpMatch {
                items,
                index,
                input,
                groups,
            } => HeapObject::RegExpMatch(RegExpMatchObject {
                items: items
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
                index: index.into_runtime()?,
                input: input.into_runtime()?,
                groups: groups.into_runtime()?,
            }),
            Self::Map { entries } => HeapObject::Map(MapObject {
                entries: entries
                    .into_iter()
                    .map(|entry| Ok((entry.key.into_runtime()?, entry.value.into_runtime()?)))
                    .collect::<Result<_, SnapshotDecodeError>>()?,
            }),
            Self::Set { values } => HeapObject::Set(SetObject {
                values: values
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
            }),
            Self::Date { milliseconds } => HeapObject::Date(DateObject {
                milliseconds: normalize_number(milliseconds),
            }),
            Self::Error {
                error_kind,
                message,
                cause,
                errors,
            } => HeapObject::Error(ErrorObject {
                kind: error_kind,
                message,
                cause: cause.map(CanonicalValue::into_runtime).transpose()?,
                errors: errors.map(CanonicalValue::into_runtime).transpose()?,
            }),
            Self::Url {
                href,
                search_params,
            } => HeapObject::Url(UrlObject {
                href,
                search_params: search_params.into_runtime()?,
            }),
            Self::UrlSearchParams { entries } => {
                HeapObject::UrlSearchParams(UrlSearchParamsObject {
                    entries: entries
                        .into_iter()
                        .map(|entry| (entry.key, entry.value))
                        .collect(),
                })
            }
        })
    }
}

impl CanonicalValue {
    pub(super) fn from_runtime(
        value: &Value,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        if depth > MAX_SNAPSHOT_VALUE_DEPTH {
            return Err(ContinuationError::UnserializableValue {
                location: location.to_string(),
                variant: "value beyond the snapshot depth limit",
            });
        }
        Ok(match value {
            Value::Null => Self::Null {},
            Value::Undefined => Self::Undefined {},
            Value::Bool(value) => Self::Bool { value: *value },
            Value::Number(value) => Self::Number {
                value: normalize_number(*value),
            },
            Value::String(value) => Self::String {
                value: value.to_string(),
            },
            Value::Image(value) => Self::Image {
                value: (**value).clone(),
            },
            Value::Resource(value) => Self::Resource {
                value: value.clone(),
            },
            Value::Ref(value) => Self::Ref { value: *value },
            Value::Tuple(values) => Self::Tuple {
                items: canonical_items(values, location, depth)?,
            },
            Value::List(values) => Self::List {
                items: canonical_items(values, location, depth)?,
            },
            Value::Record(record) => {
                let mut fields = record.iter().collect::<Vec<_>>();
                fields.sort_unstable_by_key(|(name, _)| *name);
                Self::Record {
                    fields: fields
                        .into_iter()
                        .map(|(name, value)| {
                            let location = child_location(location, name);
                            Ok(CanonicalBinding {
                                name: name.to_string(),
                                value: Self::from_runtime(value, &location, depth + 1)?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                }
            }
            Value::Projected(projected) => Self::Projected {
                value: CanonicalProjectedValue {
                    name: projected.name().to_string(),
                    type_name: projected.value_type_name().to_string(),
                    projection_ref: projected
                        .projection_ref()
                        .map(|value| {
                            CanonicalJsonValue::from_json(
                                value,
                                &format!("{location}.projection_ref"),
                                depth + 1,
                            )
                        })
                        .transpose()?,
                },
            },
        })
    }

    pub(super) fn into_runtime(self) -> Result<Value, SnapshotDecodeError> {
        Ok(match self {
            Self::Null {} => Value::Null,
            Self::Undefined {} => Value::Undefined,
            Self::Bool { value } => Value::Bool(value),
            Self::Number { value } => Value::Number(normalize_number(value)),
            Self::String { value } => Value::String(value.into()),
            Self::Image { value } => Value::Image(Box::new(value)),
            Self::Resource { value } => Value::Resource(value),
            Self::Ref { value } => Value::Ref(value),
            Self::Tuple { items } => Value::Tuple(
                items
                    .into_iter()
                    .map(Self::into_runtime)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            Self::List { items } => Value::List(
                items
                    .into_iter()
                    .map(Self::into_runtime)
                    .collect::<Result<Vec<_>, _>>()?
                    .into(),
            ),
            Self::Record { fields } => Value::Record(Arc::new(
                fields
                    .into_iter()
                    .map(|field| {
                        ensure_no_prototype_chain_wire_key(&field.name)?;
                        field.value.into_runtime().map(|value| (field.name, value))
                    })
                    .collect::<Result<_, _>>()?,
            )),
            Self::Projected { value } => Value::Projected(
                ProjectedValue::unavailable_after_restore_with_projection_ref(
                    value.name,
                    value.type_name,
                    value
                        .projection_ref
                        .map(CanonicalJsonValue::into_json)
                        .transpose()?,
                ),
            ),
        })
    }
}

fn canonical_items(
    values: &[Value],
    location: &str,
    depth: usize,
) -> Result<Vec<CanonicalValue>, ContinuationError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            CanonicalValue::from_runtime(value, &format!("{location}[{index}]"), depth + 1)
        })
        .collect()
}

impl CanonicalJsonValue {
    fn from_json(
        value: &serde_json::Value,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        if depth > MAX_SNAPSHOT_VALUE_DEPTH {
            return Err(ContinuationError::UnserializableValue {
                location: location.to_string(),
                variant: "value beyond the snapshot depth limit",
            });
        }
        Ok(match value {
            serde_json::Value::Null => Self::Null {},
            serde_json::Value::Bool(value) => Self::Bool { value: *value },
            serde_json::Value::Number(value) => Self::Number {
                value: value.clone(),
            },
            serde_json::Value::String(value) => Self::String {
                value: value.clone(),
            },
            serde_json::Value::Array(items) => Self::Array {
                items: items
                    .iter()
                    .enumerate()
                    .map(|(index, value)| {
                        Self::from_json(value, &format!("{location}[{index}]"), depth + 1)
                    })
                    .collect::<Result<_, _>>()?,
            },
            serde_json::Value::Object(fields) => {
                let mut fields = fields.iter().collect::<Vec<_>>();
                fields.sort_unstable_by_key(|(name, _)| *name);
                Self::Object {
                    fields: fields
                        .into_iter()
                        .map(|(name, value)| {
                            let location = child_location(location, name);
                            Ok(CanonicalJsonField {
                                name: name.clone(),
                                value: Self::from_json(value, &location, depth + 1)?,
                            })
                        })
                        .collect::<Result<_, ContinuationError>>()?,
                }
            }
        })
    }

    fn into_json(self) -> Result<serde_json::Value, SnapshotDecodeError> {
        Ok(match self {
            Self::Null {} => serde_json::Value::Null,
            Self::Bool { value } => serde_json::Value::Bool(value),
            Self::Number { value } => serde_json::Value::Number(value),
            Self::String { value } => serde_json::Value::String(value),
            Self::Array { items } => serde_json::Value::Array(
                items
                    .into_iter()
                    .map(Self::into_json)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Object { fields } => serde_json::Value::Object(
                fields
                    .into_iter()
                    .map(|field| field.value.into_json().map(|value| (field.name, value)))
                    .collect::<Result<_, _>>()?,
            ),
        })
    }
}

fn normalize_number(value: f64) -> f64 {
    if value.is_nan() {
        f64::from_bits(CANONICAL_NAN_BITS)
    } else {
        value
    }
}

pub(super) fn child_location(parent: &str, name: &str) -> String {
    if is_path_identifier(name) {
        format!("{parent}.{name}")
    } else {
        let quoted = serde_json::to_string(name).expect("string serialization cannot fail");
        format!("{parent}[{quoted}]")
    }
}

fn is_path_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some('_' | 'a'..='z' | 'A'..='Z'))
        && chars.all(|character| matches!(character, '_' | 'a'..='z' | 'A'..='Z' | '0'..='9'))
}

/// The value-entry guard on the third way a record can arrive from outside:
/// decoded wire bytes.
///
/// Nothing this runtime encodes can carry a prototype-chain data key any more —
/// `JSON.parse` and every host result refuse one — so a wire that does is
/// forged or from before the guard, and restoring it would recreate exactly the
/// stranded state the guard exists to prevent: a key `Object.keys` lists and
/// nothing can read or serialize.
fn ensure_no_prototype_chain_wire_key(name: &str) -> Result<(), SnapshotDecodeError> {
    if crate::runtime::access::is_prototype_chain_key(name) {
        return Err(SnapshotDecodeError::InvalidEncoding(format!(
            "record key `{name}` names the prototype chain, which this value model does not have"
        )));
    }
    Ok(())
}
