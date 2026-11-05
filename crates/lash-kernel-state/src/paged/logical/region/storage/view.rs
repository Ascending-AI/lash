//! Borrowed call views and canonical charge measurements of the current graph.
use super::*;
use lash_kernel_doc::{Measure, ValueKind};

impl StateLease<'_> {
    pub(super) fn bind_callable(
        &mut self,
        index: u32,
        handle: ArenaHandle,
    ) -> Result<(), HeapFault> {
        if self.local.slots[index as usize].existing().cold {
            return Ok(());
        }
        let data = match self.arena().body(handle).map_err(arena_fault)? {
            Body::Function(identity) => Some(LocalCallable::Function(**identity)),
            Body::Closure {
                identity, captures, ..
            } => {
                let identity = Arc::clone(identity);
                let count = captures.len();
                let mut values = Vec::new();
                capacity(&mut values, count)?;
                for at in 0..count {
                    let Body::Closure { captures, .. } =
                        self.arena().body(handle).map_err(arena_fault)?
                    else {
                        unreachable!("exclusive closure binding")
                    };
                    let handle = captures[at];
                    let value = self.import_one(&Value::ObjectRef(handle))?;
                    let cell = self.local.index(self.object(value)?)?;
                    if self.local.slots[cell].kind != Kind::Cell {
                        return Err(invalid("callable capture is not a variable cell"));
                    }
                    values.push(value);
                }
                Some(LocalCallable::Closure(LocalClosure {
                    identity,
                    captures: values,
                }))
            }
            _ => None,
        };
        self.local.slots[index as usize].existing_mut().callable = data;
        Ok(())
    }
    pub fn callable(&mut self, value: LeaseValue) -> Result<LeaseCallable<'_>, LeaseFault> {
        if self.local.frozen {
            return Err(invalid("lease graph is frozen").into());
        }
        if !matches!(self.kind(value)?, ValueKind::Function | ValueKind::Closure) {
            return Err(NativeError::Decline.into());
        }
        let index = self.local.index(self.object(value)?)?;
        let slot = &self.local.slots[index];
        if slot.existing.as_ref().is_some_and(|existing| existing.cold) {
            return Err(LeaseFault::BoundaryRequired);
        }
        let closure = match &slot.body {
            Some(LocalBody::Closure(closure)) => closure,
            _ => match slot
                .existing
                .as_ref()
                .and_then(|existing| existing.callable.as_ref())
            {
                Some(LocalCallable::Function(identity)) => {
                    return Ok(LeaseCallable {
                        identity,
                        closure: None,
                        captures: &[],
                    });
                }
                Some(LocalCallable::Closure(closure)) => closure,
                None => return Err(LeaseFault::BoundaryRequired),
            },
        };
        Ok(LeaseCallable {
            identity: &closure.identity.callable,
            closure: Some(&closure.identity),
            captures: &closure.captures,
        })
    }
    pub fn kind(&self, value: LeaseValue) -> Result<ValueKind, HeapFault> {
        Ok(match value.repr {
            Repr::Scalar(value) => value.kind(),
            Repr::Object(object, native) => {
                let index = self.local.index(object)?;
                if let Some(native) = native {
                    native.kind()
                } else {
                    match self.local.slots[index].kind {
                        Kind::List => ValueKind::List,
                        Kind::Map => ValueKind::Map,
                        Kind::Set => ValueKind::Set,
                        Kind::Record => ValueKind::Record,
                        Kind::Tuple => ValueKind::Tuple,
                        Kind::Cell => ValueKind::Ref,
                        Kind::Closure => ValueKind::Closure,
                        Kind::Text => ValueKind::Text,
                        Kind::Bytes => ValueKind::Bytes,
                        Kind::Existing => return Err(invalid("local value has no kind")),
                    }
                }
            }
        })
    }
    /// Version-qualified derived reads expire after every accepted local edit.
    pub fn version(&self, object: LeaseObject) -> Result<u64, HeapFault> {
        Ok(self.local.slots[self.local.index(object)?].content)
    }
    pub fn measure(&mut self, value: LeaseValue, selector: Measure) -> Result<u64, LeaseFault> {
        if self.local.frozen {
            return Err(invalid("lease graph is frozen").into());
        }
        match selector {
            Measure::Size => return self.shallow_measure(value),
            Measure::Magnitude => return self.magnitude_measure(value),
            _ => {}
        }
        self.local.measure_epoch = self
            .local
            .measure_epoch
            .checked_add(1)
            .ok_or_else(|| invalid("lease measurement epoch overflow"))?;
        let mut pending = std::mem::take(&mut self.local.measure_pending);
        pending.clear();
        let result = (|| {
            if matches!(selector, Measure::NestedSize) {
                self.measure_members(value, &mut pending, true)?;
            } else {
                capacity(&mut pending, 1)?;
                pending.push(value);
            }
            let mut total = 0u64;
            while let Some(value) = pending.pop() {
                let kind = self.kind(value)?;
                let mutable = matches!(
                    kind,
                    ValueKind::List | ValueKind::Map | ValueKind::Set | ValueKind::Record
                );
                if matches!(selector, Measure::NestedSize) && mutable {
                    total = total.saturating_add(1);
                    continue;
                }
                if mutable {
                    let index = self.local.index(self.object(value)?)?;
                    if self.local.slots[index].measured == self.local.measure_epoch {
                        continue;
                    }
                    self.local.slots[index].measured = self.local.measure_epoch;
                }
                total = total.saturating_add(self.shallow_measure(value)?);
                self.measure_members(value, &mut pending, matches!(selector, Measure::NestedSize))?;
            }
            Ok(total)
        })();
        pending.clear();
        self.local.measure_pending = pending;
        result
    }
    fn magnitude_measure(&self, value: LeaseValue) -> Result<u64, LeaseFault> {
        let native = match value.repr {
            Repr::Scalar(value) => Some(value),
            Repr::Object(object, native) => {
                self.local.index(object)?;
                native
            }
        };
        Ok(match native {
            Some(Value::Int(word)) => u64::try_from(word).unwrap_or(0),
            Some(Value::Float(float)) if float.get().is_finite() && float.get() >= 0.0 => {
                float.get() as u64
            }
            Some(Value::BigInt(handle)) => match self.arena().body(handle).map_err(local_native)? {
                Body::BigInt(integer) if !integer.is_negative() => {
                    if integer.bits() > 64 {
                        u64::MAX
                    } else if let Some(word) = integer.word() {
                        word as u64
                    } else {
                        integer.as_bigint().iter_u64_digits().next().unwrap_or(0)
                    }
                }
                _ => 0,
            },
            _ => 0,
        })
    }
    fn shallow_measure(&self, value: LeaseValue) -> Result<u64, LeaseFault> {
        let native = match value.repr {
            Repr::Scalar(value) => {
                return Ok(match value {
                    Value::Int(word) => 1 + u64::from(word != 0),
                    Value::Text(text) => 1 + text.as_bytes().len() as u64,
                    _ => 1,
                });
            }
            Repr::Object(object, native) => {
                let index = self.local.index(object)?;
                let slot = &self.local.slots[index];
                if let Some(existing) = &slot.existing
                    && existing.cold
                {
                    return Err(LeaseFault::BoundaryRequired);
                }
                let kind = self.kind(value)?;
                if matches!(
                    kind,
                    ValueKind::Tuple
                        | ValueKind::List
                        | ValueKind::Map
                        | ValueKind::Set
                        | ValueKind::Record
                ) {
                    let len = self.length(index);
                    return Ok(1u64.saturating_add(if kind == ValueKind::Map {
                        len.saturating_mul(2)
                    } else {
                        len
                    }));
                }
                let Some(native) = native else {
                    return Ok(match &slot.body {
                        Some(LocalBody::Text(text)) => (text.len() as u64).saturating_add(1),
                        Some(LocalBody::Bytes(bytes)) => (bytes.len() as u64).saturating_add(1),
                        _ => 1,
                    });
                };
                native
            }
        };
        let extent = match native {
            Value::BigInt(handle) => match self.arena().body(handle).map_err(local_native)? {
                Body::BigInt(integer) => integer.bits().div_ceil(64),
                _ => 0,
            },
            Value::LongText(handle) => match self.arena().body(handle).map_err(local_native)? {
                Body::LongText(text) => text.summary.utf8_bytes,
                _ => 0,
            },
            Value::Bytes(handle) => match self.arena().body(handle).map_err(local_native)? {
                Body::Bytes(bytes) => bytes.len,
                _ => 0,
            },
            Value::Error(handle) => match self.arena().body(handle).map_err(local_native)? {
                Body::Error(error) => self
                    .native_text_len(error.kind)?
                    .saturating_add(self.native_text_len(error.message)?),
                _ => 0,
            },
            _ => 0,
        };
        Ok(extent.saturating_add(1))
    }
    fn native_text_len(&self, value: Value) -> Result<u64, LeaseFault> {
        Ok(match value {
            Value::Text(text) => text.as_bytes().len() as u64,
            Value::LongText(handle) => match self.arena().body(handle).map_err(local_native)? {
                Body::LongText(text) => text.summary.utf8_bytes,
                _ => 0,
            },
            _ => 0,
        })
    }
    fn measure_members(
        &mut self,
        value: LeaseValue,
        pending: &mut Vec<LeaseValue>,
        immutable_only: bool,
    ) -> Result<(), LeaseFault> {
        let kind = self.kind(value)?;
        if !matches!(kind, ValueKind::Tuple | ValueKind::Error)
            && (immutable_only
                || !matches!(
                    kind,
                    ValueKind::List | ValueKind::Map | ValueKind::Set | ValueKind::Record
                ))
        {
            return Ok(());
        }
        let index = self.local.index(self.object(value)?)?;
        if let Some(existing) = &self.local.slots[index].existing {
            if existing.cold {
                return Err(LeaseFault::BoundaryRequired);
            }
            let handle = existing
                .value
                .and_then(|value| value.handle())
                .ok_or(LeaseFault::BoundaryRequired)?;
            if kind == ValueKind::Error {
                let Body::Error(error) = self.arena().body(handle).map_err(local_native)? else {
                    return Err(invalid("error measurement has another body").into());
                };
                let data = error.data;
                let value = self.import(&data)?;
                capacity(pending, 1)?;
                pending.push(value);
                return Ok(());
            }
            if matches!(kind, ValueKind::Tuple | ValueKind::List) {
                let len = self.length(index);
                capacity(pending, usize::try_from(len).map_err(|_| exhausted())?)?;
                for at in 0..len {
                    let value = if kind == ValueKind::List {
                        self.member_entry(index, &LeaseMember::List(at), &Address::List(at))?
                            .ok_or_else(|| invalid("measured list member is absent"))?
                            .value
                    } else {
                        let value = self
                            .arena()
                            .item(handle, at)
                            .map_err(local_native)?
                            .ok_or_else(|| invalid("measured tuple member is absent"))?;
                        self.import(&value)?
                    };
                    pending.push(value);
                }
                return Ok(());
            }
            let count = self.arena().entries(handle).map_err(local_native)?.len() / 2;
            capacity(pending, count.checked_mul(2).ok_or_else(exhausted)?)?;
            for at in 0..count {
                let entries = self.arena().entries(handle).map_err(local_native)?;
                let key = entries[at * 2];
                let value = entries[at * 2 + 1];
                let address = Address::Table(LeaseKey::Canonical(
                    self.arena().key(&key).map_err(local_native)?,
                ));
                let held = self.local.slots[index].existing().overlays.get(&address);
                let pair = if let Some(overlay) = held {
                    match (&overlay.original, &overlay.current) {
                        (Some(old), Some(new)) if old.sequence == new.sequence => {
                            Some((new.key.clone(), new.value))
                        }
                        _ => None,
                    }
                } else {
                    Some((Spelling::Value(self.import(&key)?), self.import(&value)?))
                };
                if let Some((key, value)) = pair {
                    push_pair(pending, key, value, kind)?;
                }
            }
            for overlay in self.local.slots[index].existing().overlays.values() {
                if let Some(new) = &overlay.current
                    && overlay
                        .original
                        .as_ref()
                        .is_none_or(|old| old.sequence != new.sequence)
                {
                    push_pair(pending, new.key.clone(), new.value, kind)?;
                }
            }
        } else {
            match &self.local.slots[index].body {
                Some(LocalBody::Values(values)) => {
                    capacity(pending, values.len())?;
                    pending.extend_from_slice(values);
                }
                Some(LocalBody::Table(table)) => {
                    capacity(
                        pending,
                        table.entries.len().checked_mul(2).ok_or_else(exhausted)?,
                    )?;
                    for entry in &table.entries {
                        push_pair(pending, entry.key.clone(), entry.value, kind)?;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}
fn push_pair(
    pending: &mut Vec<LeaseValue>,
    key: Spelling,
    value: LeaseValue,
    kind: ValueKind,
) -> Result<(), HeapFault> {
    capacity(pending, 2)?;
    if kind != ValueKind::Record {
        if let Spelling::Value(key) = key {
            pending.push(key);
        } else {
            return Err(invalid("map key has a record-only field spelling"));
        }
    }
    if kind != ValueKind::Set {
        pending.push(value);
    }
    Ok(())
}
