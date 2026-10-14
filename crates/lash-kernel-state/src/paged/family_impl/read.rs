//! Read-only host projection of the existing frozen occurrence grammar.
use super::*;
use lash_kernel_doc::{Bytes, Datum, ErrorDatum};

impl PagedHeap {
    pub fn frozen_datum(&self, frozen: &FrozenRef) -> Result<Datum, HeapFault> {
        let header = read_header(&self.access, frozen)?;
        project(self, &header, &header.value.0, 0)
    }
}

fn project(
    heap: &PagedHeap,
    header: &FrozenHeader,
    value: &Value,
    depth: u32,
) -> Result<Datum, HeapFault> {
    if depth >= 128 {
        return Err(invalid("frozen projection exceeds the depth bound"));
    }
    let nested = |value: &Value| project(heap, header, value, depth + 1);
    Ok(match value {
        Value::Null => Datum::Null,
        Value::Absent => Datum::Absent,
        Value::Bool(v) => Datum::Bool(*v),
        Value::Int(v) => Datum::Int(heap.copy_int(v)?),
        Value::Float(v) => Datum::Float(*v),
        Value::Text(v) => Datum::Text(heap.copy_text(v)?.to_string()),
        Value::Bytes(v) => Datum::Bytes(Bytes::new(heap.copy_bytes(v)?.to_vec())),
        Value::Timestamp(v) => Datum::Timestamp(v.clone()),
        Value::Function(v) => Datum::Function(v.clone()),
        Value::Handle(v) => Datum::Handle(v.as_ref().clone()),
        Value::Tuple(tuple) => {
            let mut values = Vec::new();
            for at in 0..tuple.summary.len {
                let value = heap
                    .tuple_get(tuple, at)?
                    .ok_or_else(|| invalid("frozen tuple member missing"))?;
                values.push(nested(&value)?);
            }
            Datum::Tuple(values)
        }
        Value::Error(error) => Datum::Error(Box::new(ErrorDatum {
            kind: heap.copy_text(&error.kind)?.to_string(),
            message: heap.copy_text(&error.message)?.to_string(),
            data: nested(&heap.error_data(error)?)?,
        })),
        Value::List(id) | Value::Map(id) | Value::Set(id) | Value::Record(id) => {
            validate_occurrence(value, header.summary.entities)?;
            let head = header
                .objects
                .get(&ordinal_key(id.ordinal), &heap.access)?
                .ok_or_else(|| invalid("frozen occurrence is missing"))?;
            match (value, head.body) {
                (Value::List(_), Body::List { inline, rows, .. }) => {
                    let mut values = Vec::new();
                    if let Some(inline) = inline {
                        for value in inline {
                            values.push(nested(&value)?);
                        }
                    } else {
                        let rows = rows.in_family(RELATIVE);
                        for at in 0..rows.len() {
                            let (_, value) = rows
                                .rank_get(at, &heap.access)?
                                .ok_or_else(|| invalid("frozen list member missing"))?;
                            values.push(nested(&value)?);
                        }
                    }
                    Datum::List(values)
                }
                (value, Body::Table { kind, rows, .. }) => {
                    let rows = rows.in_family(RELATIVE);
                    let mut entries = Vec::new();
                    let mut members = Vec::new();
                    let mut fields = Vec::new();
                    for at in 0..rows.len() {
                        let (_, entry) = rows
                            .rank_get(at, &heap.access)?
                            .ok_or_else(|| invalid("frozen table member missing"))?;
                        match (value, kind) {
                            (Value::Map(_), ObjectKind::Map) => {
                                entries.push((nested(&entry.key)?, nested(&entry.value)?))
                            }
                            (Value::Set(_), ObjectKind::Set) => members.push(nested(&entry.key)?),
                            (Value::Record(_), ObjectKind::Record) => {
                                let Value::Text(name) = entry.key else {
                                    return Err(invalid("frozen record name is not text"));
                                };
                                fields.push((
                                    heap.copy_text(&name)?.to_string(),
                                    nested(&entry.value)?,
                                ));
                            }
                            _ => {
                                return Err(invalid(
                                    "frozen table kind differs from its reference",
                                ));
                            }
                        }
                    }
                    match value {
                        Value::Map(_) => Datum::Map(entries),
                        Value::Set(_) => Datum::Set(members),
                        _ => Datum::Record(fields),
                    }
                }
                _ => return Err(invalid("frozen occurrence kind differs from its reference")),
            }
        }
        _ => return Err(invalid("frozen projection contains a runtime identity")),
    })
}
