//! A native call's answer, adopted: the graph the native built enters the
//! lease as fresh slots that only the call's own phase accepts
//! ([`LeaseOp::Adopt`]). Until then no other phase is accepted, and a lease
//! that leaves first retires them, so the canonical state never holds them.
use super::*;
use lash_kernel_doc::NativeResourceContract;
use lash_kernel_doc::NewObject;
use std::collections::HashMap;

/// What one native call built, detached from the arena: its objects in the
/// order it built them, named by the slots the call reserved and gave back
/// ([`lash_kernel_doc::arena::Nursery::detach`]); its answer; the balances of
/// the contract that certifies the call, if one does; the logical bytes its
/// nursery held at most; and the operands it was called with. Nothing
/// installs it. A lease adopts it, which consumes it, or it is dropped,
/// which leaves no trace.
pub struct NativeBatch {
    objects: Vec<(ArenaHandle, NewObject)>,
    value: Value,
    certified: Option<(u64, u64, u64)>,
    held: u64,
    operands: Box<[Value]>,
}
impl NativeBatch {
    pub fn new(
        objects: Vec<(ArenaHandle, NewObject)>,
        value: Value,
        contract: Option<&NativeResourceContract>,
        held: u64,
        operands: Box<[Value]>,
    ) -> Result<Self, HeapFault> {
        let certified = contract
            .map(|contract| {
                contract
                    .balances()
                    .ok_or_else(|| invalid("a native's resource contract does not balance"))
            })
            .transpose()?;
        Ok(Self {
            objects,
            value,
            certified,
            held,
            operands,
        })
    }
    /// The certifying contract's retained delta, peak and scratch peak.
    pub fn certified(&self) -> Option<(u64, u64, u64)> {
        self.certified
    }
    /// The logical bytes the call's nursery held at most.
    pub fn held(&self) -> u64 {
        self.held
    }
    /// The answer by its kind: a built object's handle names no arena slot.
    pub fn answer(&self) -> Value {
        self.value
    }
    /// The answer's immutable nesting depth, a value the call did not build
    /// at its `outside` depth.
    pub fn depth(&self, outside: &dyn Fn(&Value) -> u32) -> u32 {
        let labels: Labels = self
            .objects
            .iter()
            .enumerate()
            .map(|(at, (handle, _))| ((handle.index(), handle.generation()), at))
            .collect();
        let at = |value: &Value| {
            let handle = value.handle()?;
            labels.get(&(handle.index(), handle.generation())).copied()
        };
        let mut depths: Vec<u32> = Vec::with_capacity(self.objects.len());
        for (index, (_, object)) in self.objects.iter().enumerate() {
            let mut depth = 0u32;
            members(object, &mut |member| {
                depth = depth.max(match at(member) {
                    Some(inner) if inner < index => depths[inner],
                    _ => outside(member),
                });
            });
            depths.push(match object {
                NewObject::Tuple(_) | NewObject::Error { .. } => depth.saturating_add(1),
                _ => 0,
            });
        }
        match at(&self.value) {
            Some(index) => depths[index],
            None => outside(&self.value),
        }
    }
}
/// A built object's batch position, by its label.
type Labels = HashMap<(u32, u32), usize, foldhash::fast::RandomState>;

impl StateLease<'_> {
    /// Adopts what a native called with `operands` built: the objects its
    /// answer reaches become fresh slots, priced as their constructors price
    /// them, and its answer is returned with its immutable nesting depth and
    /// the bytes of the mutable bodies adopted. What the answer does not
    /// reach left before the call returned.
    pub fn adopt(
        &mut self,
        batch: NativeBatch,
        operands: &[LeaseValue],
    ) -> Result<(LeaseValue, u32, u64), LeaseFault> {
        if self.local.frozen {
            return Err(invalid("lease graph is frozen").into());
        }
        if self.local.adopted.is_some() {
            return Err(invalid("a native answer is already adopted").into());
        }
        if operands.len() != batch.operands.len()
            || operands
                .iter()
                .zip(&batch.operands)
                .any(|(lease, value)| match lease.repr {
                    Repr::Scalar(actual) | Repr::Object(_, Some(actual)) => !actual.same(value),
                    Repr::Object(_, None) => true,
                })
        {
            return Err(invalid("a native answer is adopted by a call with other operands").into());
        }
        let NativeBatch { objects, value, .. } = batch;
        let mut labels = Labels::default();
        labels.try_reserve(objects.len()).map_err(|_| exhausted())?;
        for (at, (handle, _)) in objects.iter().enumerate() {
            if labels
                .insert((handle.index(), handle.generation()), at)
                .is_some()
            {
                return Err(invalid("a native batch names one slot twice").into());
            }
        }
        let label = |value: &Value| -> Result<Option<usize>, HeapFault> {
            let Some(handle) = value.handle() else {
                return Ok(None);
            };
            let Some(&at) = labels.get(&(handle.index(), handle.generation())) else {
                return Ok(None);
            };
            if std::mem::discriminant(value)
                != std::mem::discriminant(&objects[at].1.naming()(handle))
            {
                return Err(invalid("a native batch names an object by another kind"));
            }
            Ok(Some(at))
        };
        // Built bottom-up: a holder follows what it holds, so one pass from
        // the last object marks everything the answer reaches.
        let mut reached = Vec::new();
        capacity(&mut reached, objects.len())?;
        reached.resize(objects.len(), false);
        if let Some(at) = label(&value)? {
            reached[at] = true;
        }
        for at in (0..objects.len()).rev() {
            if !reached[at] {
                continue;
            }
            let mut held = Ok(());
            members(&objects[at].1, &mut |member| {
                if held.is_err() {
                    return;
                }
                match label(member) {
                    Ok(Some(inner)) if inner < at => reached[inner] = true,
                    Ok(Some(_)) => held = Err(invalid("a native batch holds what it built later")),
                    Ok(None) => {}
                    Err(error) => held = Err(error),
                }
            });
            held?;
        }
        let mut adopted = Vec::new();
        capacity(
            &mut adopted,
            reached.iter().filter(|reached| **reached).count(),
        )?;
        let mut built: Vec<Option<(LeaseValue, u32)>> = Vec::new();
        capacity(&mut built, objects.len())?;
        built.resize(objects.len(), None);
        let answer = (|| {
            for (at, (_, object)) in objects.iter().enumerate() {
                if reached[at] {
                    let slot = self.adopt_object(object, &label, &built)?;
                    if let Repr::Object(object, _) = slot.0.repr {
                        adopted.push(object.slot);
                    }
                    built[at] = Some(slot);
                }
            }
            self.adopted_value(&value, &label, &built)
        })();
        match answer {
            Ok((answer, depth)) => {
                let bodies = adopted
                    .iter()
                    .map(|&index| &self.local.slots[index as usize])
                    .filter(|slot| slot.kind.mutable())
                    .try_fold(0u64, |sum, slot| sum.checked_add(slot.bytes));
                let Some(bodies) = bodies else {
                    self.retire_adopted(&adopted);
                    return Err(invalid("adopted body cost overflow").into());
                };
                self.local.adopted = Some(adopted);
                Ok((answer, depth, bodies))
            }
            Err(error) => {
                self.retire_adopted(&adopted);
                Err(error)
            }
        }
    }
    /// Retires slots an adoption added that no phase accepted.
    pub(in crate::paged::logical::region) fn retire_adopted(&mut self, adopted: &[u32]) {
        for &index in adopted.iter().rev() {
            let slot = &mut self.local.slots[index as usize];
            slot.live = false;
            if let Some(body) = &mut slot.body {
                body.clear();
            }
            self.local.free.push(index);
        }
    }
    fn adopted_value(
        &mut self,
        value: &Value,
        label: &impl Fn(&Value) -> Result<Option<usize>, HeapFault>,
        built: &[Option<(LeaseValue, u32)>],
    ) -> Result<(LeaseValue, u32), LeaseFault> {
        if let Some(at) = label(value)? {
            return built[at].ok_or_else(|| invalid("a native batch answer was not built").into());
        }
        let value = self.import(value)?;
        let depth = match value.repr {
            Repr::Object(object, _) => self.local.slots[self.local.index(object)?].depth,
            Repr::Scalar(_) => 0,
        };
        Ok((value, depth))
    }
    /// `source`'s values, their summed occurrence price and deepest nesting.
    fn adopted_values(
        &mut self,
        source: &[Value],
        label: &impl Fn(&Value) -> Result<Option<usize>, HeapFault>,
        built: &[Option<(LeaseValue, u32)>],
    ) -> Result<(Vec<LeaseValue>, u64, u32), LeaseFault> {
        let mut values = Vec::new();
        capacity(&mut values, source.len())?;
        let mut bytes = 0;
        let mut depth = 0;
        for member in source {
            let (value, inner) = self.adopted_value(member, label, built)?;
            bytes = plus(bytes, self.price(value)?)?;
            depth = depth.max(inner);
            values.push(value);
        }
        Ok((values, bytes, depth))
    }
    fn adopt_object(
        &mut self,
        object: &NewObject,
        label: &impl Fn(&Value) -> Result<Option<usize>, HeapFault>,
        built: &[Option<(LeaseValue, u32)>],
    ) -> Result<(LeaseValue, u32), LeaseFault> {
        let nested = |depth: u32| {
            depth
                .checked_add(1)
                .ok_or_else(|| invalid("immutable nesting depth overflow"))
        };
        let (kind, body, bytes, depth) = match object {
            NewObject::List(source) => {
                let (values, bytes, _) = self.adopted_values(source, label, built)?;
                (
                    Kind::List,
                    LocalBody::Values(values),
                    plus(Cost::OBJECT, bytes)?,
                    0,
                )
            }
            NewObject::Tuple(source) => {
                let (values, bytes, depth) = self.adopted_values(source, label, built)?;
                (
                    Kind::Tuple,
                    LocalBody::Values(values),
                    plus(Cost::REFERENCE, bytes)?,
                    nested(depth)?,
                )
            }
            NewObject::Error {
                kind,
                message,
                data,
            } => {
                let (values, _, depth) =
                    self.adopted_values(&[*kind, *message, *data], label, built)?;
                if !self.is_text(values[0])? || !self.is_text(values[1])? {
                    return Err(invalid("a built error's kind or message is not text").into());
                }
                // Its texts' payloads and its data's whole occurrence.
                let bytes = plus(
                    plus(
                        self.price(values[0])?
                            .checked_sub(Cost::REFERENCE)
                            .ok_or_else(|| invalid("error text cost underflow"))?,
                        self.price(values[1])?
                            .checked_sub(Cost::REFERENCE)
                            .ok_or_else(|| invalid("error text cost underflow"))?,
                    )?,
                    self.price(values[2])?,
                )?;
                (
                    Kind::Leaf(Leaf::Error),
                    LocalBody::Values(values),
                    plus(Cost::REFERENCE, bytes)?,
                    nested(depth)?,
                )
            }
            NewObject::Map(entries) | NewObject::Record(entries) => {
                let record = matches!(object, NewObject::Record(_));
                let mut rows = Vec::new();
                capacity(&mut rows, entries.len())?;
                for (key, value) in entries {
                    rows.push((
                        self.adopted_value(key, label, built)?.0,
                        self.adopted_value(value, label, built)?.0,
                    ));
                }
                let (table, bytes) = self.adopted_table(rows, record)?;
                let kind = if record { Kind::Record } else { Kind::Map };
                (kind, LocalBody::Table(table), plus(Cost::OBJECT, bytes)?, 0)
            }
            NewObject::Set(keys) => {
                let mut rows = Vec::new();
                capacity(&mut rows, keys.len())?;
                for key in keys {
                    rows.push((
                        self.adopted_value(key, label, built)?.0,
                        LeaseValue {
                            repr: Repr::Scalar(Value::Absent),
                        },
                    ));
                }
                let (table, bytes) = self.adopted_table(rows, false)?;
                (
                    Kind::Set,
                    LocalBody::Table(table),
                    plus(Cost::OBJECT, bytes)?,
                    0,
                )
            }
            NewObject::LongText(text) => {
                let mut body = String::new();
                body.try_reserve(text.len()).map_err(|_| exhausted())?;
                body.push_str(text);
                let bytes = plus(Cost::REFERENCE, count(text.len())?)?;
                (Kind::Text, LocalBody::Text(body), bytes, 0)
            }
            NewObject::Bytes(source) => {
                let mut body = Vec::new();
                capacity(&mut body, source.len())?;
                body.extend_from_slice(source);
                let bytes = plus(Cost::REFERENCE, count(source.len())?)?;
                (Kind::Bytes, LocalBody::Bytes(body), bytes, 0)
            }
            NewObject::BigInt(integer) | NewObject::Timestamp(integer) => {
                let leaf = if matches!(object, NewObject::BigInt(_)) {
                    Leaf::BigInt
                } else {
                    Leaf::Timestamp
                };
                let bytes = plus(Cost::REFERENCE, integer.bits().div_ceil(8))?;
                (
                    Kind::Leaf(leaf),
                    LocalBody::Leaf(LeafBody::Integer(integer.clone())),
                    bytes,
                    0,
                )
            }
            NewObject::Function(callable) => (
                Kind::Leaf(Leaf::Function),
                LocalBody::Leaf(LeafBody::Function(*callable)),
                plus(Cost::REFERENCE, 64)?,
                0,
            ),
        };
        let object = self.local.empty_slot()?;
        capacity(&mut self.local.slots, 1)?;
        let slot = ObjectSlot {
            generation: object.generation,
            ordinal: None,
            kind,
            body: Some(body),
            existing: None,
            bytes,
            incoming: 0,
            live: true,
            content: 0,
            structural: 0,
            draft_epoch: 0,
            draft: 0,
            retiring: false,
            measured: 0,
            depth,
        };
        if (object.slot as usize) < self.local.slots.len() {
            self.local.free.pop();
            self.local.slots[object.slot as usize] = slot;
        } else {
            self.local.slots.push(slot);
        }
        Ok((
            LeaseValue {
                repr: Repr::Object(object, None),
            },
            depth,
        ))
    }
    /// A built table's rows, in the order the native gave them, and their
    /// price. A native gives each key once.
    fn adopted_table(
        &self,
        rows: Vec<(LeaseValue, LeaseValue)>,
        record: bool,
    ) -> Result<(LocalTable, u64), LeaseFault> {
        let mut entries = Vec::new();
        capacity(&mut entries, rows.len())?;
        let mut index = std::collections::HashMap::default();
        index.try_reserve(rows.len()).map_err(|_| exhausted())?;
        let mut bytes = 0;
        for (row, (key, value)) in rows.into_iter().enumerate() {
            if record && !self.is_text(key)? {
                return Err(invalid("a built record has a key that is not text").into());
            }
            if index.insert(self.key(key)?, row).is_some() {
                return Err(invalid("a built table repeats a key").into());
            }
            let entry = Entry {
                key: Spelling::Value(key),
                value,
                sequence: row as u64,
            };
            bytes = plus(bytes, self.entry_price(&entry, record)?)?;
            entries.push(entry);
        }
        let next = entries.len() as u64;
        Ok((
            LocalTable {
                entries,
                index,
                next,
            },
            bytes,
        ))
    }
}
/// Gives each value `object` holds.
fn members(object: &NewObject, each: &mut dyn FnMut(&Value)) {
    match object {
        NewObject::List(values) | NewObject::Tuple(values) | NewObject::Set(values) => {
            values.iter().for_each(each)
        }
        NewObject::Map(entries) | NewObject::Record(entries) => {
            for (key, value) in entries {
                each(key);
                each(value);
            }
        }
        NewObject::Error {
            kind,
            message,
            data,
        } => {
            each(kind);
            each(message);
            each(data);
        }
        NewObject::LongText(_)
        | NewObject::Function(_)
        | NewObject::Bytes(_)
        | NewObject::BigInt(_)
        | NewObject::Timestamp(_) => {}
    }
}
