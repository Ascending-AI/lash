//! Foreign input is converted once directly to frozen pages. Delivery uses
//! the resulting authenticated summary and never allocates eager objects.
use super::*;
use lash_kernel_doc::Datum;
use lash_kernel_doc::heap_values::{integer_digits, text_summary};
impl PagedHeap {
    pub fn freeze_datum(&mut self, datum: &Datum) -> Result<FrozenRef, HeapWriteError> {
        let named = BTreeMap::new();
        let mut writer = Freezer {
            heap: self,
            objects: Tree::new(false),
            active: BTreeSet::new(),
            entities: 0,
            cost: 0,
            charge: 0,
            depth: 0,
            named: &named,
            functions: BTreeSet::new(),
        };
        let value = writer.datum(datum)?;
        writer.finish(value).map(|(frozen, _)| frozen)
    }
}
fn text(access: &Access<'_>, text: &str) -> Result<TextRef, HeapFault> {
    Ok(TextRef {
        summary: text_summary(text),
        body: if text.len() as u64 <= IMMUTABLE_INLINE_MAX {
            Payload::Resident(Arc::from(text))
        } else {
            Payload::Paged(super::super::payload::build(access, text.as_bytes(), true)?)
        },
    })
}
fn bytes(access: &Access<'_>, bytes: &[u8]) -> Result<BytesRef, HeapFault> {
    Ok(BytesRef {
        len: bytes.len() as u64,
        body: if bytes.len() as u64 <= IMMUTABLE_INLINE_MAX {
            Payload::Resident(Arc::from(bytes))
        } else {
            Payload::Paged(super::super::payload::build(access, bytes, false)?)
        },
    })
}
fn has_edges(value: &Value) -> bool {
    match value {
        Value::List(_) | Value::Map(_) | Value::Set(_) | Value::Record(_) => true,
        Value::Tuple(t) => t.summary.has_edges,
        Value::Error(e) => e.has_edges,
        _ => false,
    }
}
impl Freezer<'_> {
    fn datum(&mut self, datum: &Datum) -> Result<Value, HeapWriteError> {
        if self.depth >= 128 {
            return Err(HeapWriteError::Raised {
                kind: "depth",
                message: "effect data exceeds the depth bound".into(),
            });
        }
        self.depth += 1;
        self.charge = plus(self.charge, datum_size(datum)?)?;
        let value = match datum {
            Datum::Null => Value::Null,
            Datum::Absent => Value::Absent,
            Datum::Bool(v) => Value::Bool(*v),
            Datum::Float(v) => Value::Float(*v),
            Datum::Int(v) => Value::Int(if v.bits().div_ceil(8) <= IMMUTABLE_INLINE_MAX {
                IntRef::Inline(v.clone())
            } else {
                IntRef::Paged {
                    summary: IntSummary {
                        bits: v.bits(),
                        negative: v.is_negative(),
                    },
                    digits: super::super::payload::build(
                        &self.heap.access,
                        &integer_digits(v),
                        false,
                    )?,
                }
            }),
            Datum::Number(_) => {
                return Err(HeapFault::Malformed(
                    "a number token must be decoded before freezing".into(),
                )
                .into());
            }
            Datum::Text(v) => Value::Text(text(&self.heap.access, v)?),
            Datum::Bytes(v) => Value::Bytes(bytes(&self.heap.access, v.as_slice())?),
            Datum::Timestamp(v) => Value::Timestamp(v.clone()),
            Datum::Function(v) => Value::Function(v.clone()),
            Datum::Handle(v) => Value::Handle(Arc::new(v.clone())),
            Datum::Tuple(values) => {
                let mut rows = Tree::new(true);
                let mut edges = false;
                let mut depth = 1;
                for (i, datum) in values.iter().enumerate() {
                    let value = self.datum(datum)?;
                    edges |= has_edges(&value);
                    depth = depth.max(super::super::heap_impl::value_depth(&value) + 1);
                    rows.write(
                        ordinal_key(i as u64),
                        RelativeValue(value),
                        &self.heap.access,
                    )?;
                }
                rows.seal(&self.heap.access, &mut SealWork::new(budget()))?;
                Value::Tuple(TupleRef {
                    family: None,
                    summary: TupleSummary {
                        task_edges: 0,
                        len: rows.len(),
                        cost: LogicalBytes(rows.cost()),
                        has_edges: edges,
                        depth,
                    },
                    body: rows
                        .page()
                        .map_or_else(|| Payload::Resident(Arc::from([])), Payload::Paged),
                })
            }
            Datum::List(values) | Datum::Set(values) => {
                let ordinal = self.entities;
                self.entities = plus(self.entities, 1)?;
                if matches!(datum, Datum::List(_)) {
                    let mut rows = Tree::new(true);
                    for (i, datum) in values.iter().enumerate() {
                        let value = self.datum(datum)?;
                        rows.write(
                            ordinal_key(i as u64),
                            RelativeValue(value),
                            &self.heap.access,
                        )?;
                    }
                    rows.seal(&self.heap.access, &mut SealWork::new(budget()))?;
                    let next = rows.len();
                    self.frozen_object(
                        ordinal,
                        Body::List {
                            inline: None,
                            rows: Tree::open(rows.page(), next, LogicalBytes(rows.cost()), true),
                            next,
                        },
                    )?
                } else {
                    let mut lookup = Tree::new(false);
                    let mut rows = Tree::new(true);
                    for datum in values {
                        let charge = self.charge;
                        let key = self.datum(datum)?;
                        let normal = self.normal(&key)?;
                        if lookup.get(&normal, &self.heap.access)?.is_some() {
                            self.charge = charge
                                .checked_sub(1)
                                .ok_or_else(|| invalid("set charge mismatch"))?;
                        } else {
                            self.table_entry(&mut lookup, &mut rows, key, Value::Null)?;
                        }
                    }
                    self.frozen_table(ordinal, ObjectKind::Set, lookup, rows)?
                }
            }
            Datum::Map(entries) => {
                let ordinal = self.entities;
                self.entities = plus(self.entities, 1)?;
                let mut selected: Vec<(Value, &Datum)> = Vec::new();
                let mut positions: BTreeMap<Vec<u8>, usize> = BTreeMap::new();
                for (k, v) in entries {
                    let charge = self.charge;
                    let key = self.datum(k)?;
                    let normal = self.normal(&key)?;
                    if let Some(index) = positions.get(&normal).copied() {
                        self.charge = charge;
                        selected[index] = (selected[index].0.clone(), v);
                    } else {
                        positions.insert(normal, selected.len());
                        selected.push((key, v));
                    }
                }
                self.charge = self
                    .charge
                    .checked_sub(2 * (entries.len() - selected.len()) as u64)
                    .ok_or_else(|| invalid("map charge mismatch"))?;
                let mut lookup = Tree::new(false);
                let mut rows = Tree::new(true);
                for (key, datum) in selected {
                    let value = self.datum(datum)?;
                    self.table_entry(&mut lookup, &mut rows, key, value)?;
                }
                self.frozen_table(ordinal, ObjectKind::Map, lookup, rows)?
            }
            Datum::Record(entries) => {
                let ordinal = self.entities;
                self.entities = plus(self.entities, 1)?;
                let mut selected: Vec<(&str, &Datum)> = Vec::new();
                let mut positions: BTreeMap<&str, usize> = BTreeMap::new();
                for (k, v) in entries {
                    if let Some(index) = positions.get(k.as_str()).copied() {
                        selected[index] = (selected[index].0, v);
                    } else {
                        positions.insert(k.as_str(), selected.len());
                        selected.push((k.as_str(), v));
                    }
                }
                self.charge = self
                    .charge
                    .checked_sub((entries.len() - selected.len()) as u64)
                    .ok_or_else(|| invalid("record charge mismatch"))?;
                let mut lookup = Tree::new(false);
                let mut rows = Tree::new(true);
                for (k, v) in selected {
                    let key = Value::Text(text(&self.heap.access, k)?);
                    let value = self.datum(v)?;
                    self.table_entry(&mut lookup, &mut rows, key, value)?;
                }
                self.frozen_table(ordinal, ObjectKind::Record, lookup, rows)?
            }
            Datum::Error(e) => {
                let kind = text(&self.heap.access, &e.kind)?;
                let message = text(&self.heap.access, &e.message)?;
                let data = self.datum(&e.data)?;
                Value::Error(Arc::new(frozen_error(
                    &self.heap.access,
                    kind,
                    message,
                    data,
                )?))
            }
        };
        self.depth -= 1;
        Ok(value)
    }
    fn frozen_object(&mut self, ordinal: u64, body: Body) -> Result<Value, HeapWriteError> {
        let head = ObjectHead {
            family: FamilyId::PRIMARY,
            body,
        };
        let kind = head.kind();
        self.cost = plus(self.cost, head.cost())?;
        self.objects
            .write(ordinal_key(ordinal), head, &self.heap.access)?;
        let id = EntityId {
            family: RELATIVE,
            ordinal,
        };
        Ok(match kind {
            ObjectKind::List => Value::List(id),
            ObjectKind::Map => Value::Map(id),
            ObjectKind::Set => Value::Set(id),
            _ => Value::Record(id),
        })
    }
    fn normal(&self, key: &Value) -> Result<Vec<u8>, HeapWriteError> {
        let normal = self
            .heap
            .normal_key(key)?
            .ok_or_else(|| HeapWriteError::Raised {
                kind: "invalid_key",
                message: "effect data contains an illegal key".into(),
            })?;
        Ok(match normal {
            NormalKey::Inline(v) => [vec![0], v].concat(),
            NormalKey::Digest(v) => [vec![1], v.to_vec()].concat(),
        })
    }
    fn table_entry(
        &self,
        lookup: &mut Tree<Ordinal>,
        rows: &mut Tree<RelativeEntry>,
        key: Value,
        value: Value,
    ) -> Result<(), HeapWriteError> {
        let normal = self
            .heap
            .normal_key(&key)?
            .ok_or_else(|| HeapWriteError::Raised {
                kind: "invalid_key",
                message: "effect data contains an illegal key".into(),
            })?;
        let normal = match normal {
            NormalKey::Inline(v) => [vec![0], v].concat(),
            NormalKey::Digest(v) => [vec![1], v.to_vec()].concat(),
        };
        if let Some(Ordinal(ordinal)) = lookup.get(&normal, &self.heap.access)? {
            let mut row = rows
                .get(&ordinal_key(ordinal), &self.heap.access)?
                .ok_or_else(|| invalid("frozen lookup has no row"))?;
            row.0.value = value;
            rows.write(ordinal_key(ordinal), row, &self.heap.access)?;
        } else {
            let ordinal = rows.len();
            lookup.write(normal, Ordinal(ordinal), &self.heap.access)?;
            rows.write(
                ordinal_key(ordinal),
                RelativeEntry(Entry { key, value }),
                &self.heap.access,
            )?;
        }
        Ok(())
    }
    fn frozen_table(
        &mut self,
        ordinal: u64,
        kind: ObjectKind,
        mut lookup: Tree<Ordinal>,
        mut rows: Tree<RelativeEntry>,
    ) -> Result<Value, HeapWriteError> {
        lookup.seal(&self.heap.access, &mut SealWork::new(budget()))?;
        rows.seal(&self.heap.access, &mut SealWork::new(budget()))?;
        self.frozen_object(
            ordinal,
            Body::Table {
                kind,
                lookup,
                next: rows.len(),
                rows: Tree::open(rows.page(), rows.len(), LogicalBytes(rows.cost()), true),
            },
        )
    }
}

fn datum_size(datum: &Datum) -> Result<u64, HeapFault> {
    let extent = match datum {
        Datum::Int(i) => i.bits().div_ceil(64),
        Datum::Text(t) => t.len() as u64,
        Datum::Bytes(b) => b.as_slice().len() as u64,
        Datum::Tuple(v) | Datum::List(v) | Datum::Set(v) => v.len() as u64,
        Datum::Map(v) => (v.len() as u64)
            .checked_mul(2)
            .ok_or_else(|| invalid("map extent overflow"))?,
        Datum::Record(v) => v.len() as u64,
        Datum::Error(e) => plus(e.kind.len() as u64, e.message.len() as u64)?,
        _ => 0,
    };
    plus(extent, 1)
}
