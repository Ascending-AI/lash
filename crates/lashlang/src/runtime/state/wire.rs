use super::*;
use crate::runtime::RegExpMatchObject;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(super) enum CanonicalHeapObject {
    Tuple {
        items: Vec<CanonicalValue>,
    },
    List {
        items: Vec<CanonicalValue>,
    },
    Record {
        fields: Vec<CanonicalBinding>,
    },
    Closure {
        function: u32,
        captures: Vec<CanonicalValue>,
        name: Option<CanonicalValue>,
        length: Option<CanonicalValue>,
    },
    /// A built-in value, named by its owner scope and ECMA `name`.
    BuiltinFunction {
        owner: String,
        name: String,
    },
    RegExp {
        pattern: String,
        flags: String,
        last_index: u64,
    },
    RegExpMatch {
        items: Vec<CanonicalValue>,
        index: CanonicalValue,
        input: CanonicalValue,
        groups: CanonicalValue,
    },
    Map {
        entries: Vec<CanonicalMapEntry>,
    },
    Set {
        values: Vec<CanonicalValue>,
    },
    Date {
        milliseconds: f64,
    },
    Error {
        error_kind: ErrorKind,
        message: Option<String>,
        cause: Option<CanonicalValue>,
        errors: Option<CanonicalValue>,
    },
    Url {
        href: String,
        search_params: CanonicalValue,
    },
    UrlSearchParams {
        entries: Vec<CanonicalUrlSearchParamsEntry>,
    },
}

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
            // Property order is observable, so a record's fields are written
            // in the order the object holds them, never sorted (FIG-3606).
            HeapObject::Record(record) => Self::Record {
                fields: record
                    .iter()
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
            },
            HeapObject::Closure {
                function,
                captures,
                name,
                length,
            } => Self::Closure {
                function: *function,
                captures: canonical_items(captures, &location, 0)?,
                name: name
                    .as_ref()
                    .map(|value| {
                        CanonicalValue::from_runtime(value, &format!("{location}.name"), 0)
                    })
                    .transpose()?,
                length: length
                    .as_ref()
                    .map(|value| {
                        CanonicalValue::from_runtime(value, &format!("{location}.length"), 0)
                    })
                    .transpose()?,
            },
            HeapObject::BuiltinFunction(function) => Self::BuiltinFunction {
                owner: function.owner().name().to_string(),
                name: function.name().to_string(),
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
                        crate::runtime::access::ensure_no_prototype_chain_wire_key(&field.name)
                            .map_err(|reason| {
                                SnapshotDecodeError::InvalidEncoding(reason.to_string())
                            })?;
                        field.value.into_runtime().map(|value| (field.name, value))
                    })
                    .collect::<Result<_, _>>()?,
            )),
            Self::Closure {
                function,
                captures,
                name,
                length,
            } => HeapObject::Closure {
                function,
                captures: captures
                    .into_iter()
                    .map(CanonicalValue::into_runtime)
                    .collect::<Result<_, _>>()?,
                name: name.map(CanonicalValue::into_runtime).transpose()?,
                length: length.map(CanonicalValue::into_runtime).transpose()?,
            },
            Self::BuiltinFunction { owner, name } => HeapObject::BuiltinFunction(
                crate::runtime::heap::BuiltinFunction::named_scoped(&owner, &name).ok_or_else(
                    || {
                        SnapshotDecodeError::InvalidEncoding(format!(
                            "unknown built-in function {owner}.{name}"
                        ))
                    },
                )?,
            ),
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
    pub(super) fn ensure_heapless(&self, location: &str) -> Result<(), SnapshotDecodeError> {
        match self {
            Self::Ref { .. } => {
                return Err(SnapshotDecodeError::HeaplessSnapshotContainsReference {
                    location: location.to_string(),
                });
            }
            Self::Tuple { items } | Self::List { items } => {
                for (index, value) in items.iter().enumerate() {
                    value.ensure_heapless(&format!("{location}[{index}]"))?;
                }
            }
            Self::Record { fields } => {
                for field in fields {
                    field
                        .value
                        .ensure_heapless(&child_location(location, &field.name))?;
                }
            }
            Self::Null {}
            | Self::Undefined {}
            | Self::Bool { .. }
            | Self::Number { .. }
            | Self::String { .. }
            | Self::Image { .. }
            | Self::Resource { .. }
            | Self::Projected { .. } => {}
        }
        Ok(())
    }

    pub(super) fn from_heapless_runtime(
        value: &Value,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        Self::from_runtime_with_references(value, location, depth, false)
    }

    pub(super) fn from_runtime(
        value: &Value,
        location: &str,
        depth: usize,
    ) -> Result<Self, ContinuationError> {
        Self::from_runtime_with_references(value, location, depth, true)
    }

    fn from_runtime_with_references(
        value: &Value,
        location: &str,
        depth: usize,
        references_allowed: bool,
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
            Value::Ref(value) if references_allowed => Self::Ref { value: *value },
            Value::Ref(_) => {
                return Err(ContinuationError::HeaplessSnapshotContainsReference {
                    location: location.to_string(),
                });
            }
            Value::Tuple(values) => Self::Tuple {
                items: canonical_items_with_references(
                    values,
                    location,
                    depth,
                    references_allowed,
                )?,
            },
            Value::List(values) => Self::List {
                items: canonical_items_with_references(
                    values,
                    location,
                    depth,
                    references_allowed,
                )?,
            },
            // In property order, like a heap record (FIG-3606).
            Value::Record(record) => Self::Record {
                fields: record
                    .iter()
                    .map(|(name, value)| {
                        let location = child_location(location, name);
                        Ok(CanonicalBinding {
                            name: name.to_string(),
                            value: Self::from_runtime_with_references(
                                value,
                                &location,
                                depth + 1,
                                references_allowed,
                            )?,
                        })
                    })
                    .collect::<Result<_, ContinuationError>>()?,
            },
            Value::Projected(projected) => Self::Projected {
                value: CanonicalProjectedValue::from_projected(projected, location, depth)?,
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
                        crate::runtime::access::ensure_no_prototype_chain_wire_key(&field.name)
                            .map_err(|reason| {
                                SnapshotDecodeError::InvalidEncoding(reason.to_string())
                            })?;
                        field.value.into_runtime().map(|value| (field.name, value))
                    })
                    .collect::<Result<_, _>>()?,
            )),
            Self::Projected { value } => Value::Projected(value.into_projected()?),
        })
    }
}

fn canonical_items(
    values: &[Value],
    location: &str,
    depth: usize,
) -> Result<Vec<CanonicalValue>, ContinuationError> {
    canonical_items_with_references(values, location, depth, true)
}

fn canonical_items_with_references(
    values: &[Value],
    location: &str,
    depth: usize,
    references_allowed: bool,
) -> Result<Vec<CanonicalValue>, ContinuationError> {
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            CanonicalValue::from_runtime_with_references(
                value,
                &format!("{location}[{index}]"),
                depth + 1,
                references_allowed,
            )
        })
        .collect()
}

fn normalize_number(value: f64) -> f64 {
    if value.is_nan() {
        f64::from_bits(CANONICAL_NAN_BITS)
    } else {
        value
    }
}

#[expect(
    clippy::expect_used,
    reason = "serde_json::to_string of a plain string cannot fail, per the message"
)]
pub(crate) fn child_location(parent: &str, name: &str) -> String {
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
