use super::*;

impl<'s> StateLease<'s> {
    pub fn import(&mut self, value: &Value) -> Result<LeaseValue, HeapFault> {
        #[cfg(any(test, feature = "perf-witness"))]
        {
            self.local.binding_depth += 1;
            self.local.binding_peak = self.local.binding_peak.max(self.local.binding_depth);
        }
        let result = self.import_one(value).and_then(|value| {
            while let Some(binding) = self.local.bindings.pop() {
                match binding {
                    Binding::Edges(index, entity, value) => {
                        self.bind_edges(index, entity, value)?
                    }
                    Binding::Callable(index, handle) => self.bind_callable(index, handle)?,
                }
            }
            Ok(value)
        });
        #[cfg(any(test, feature = "perf-witness"))]
        {
            self.local.binding_depth -= 1;
        }
        result
    }
    pub(super) fn import_one(&mut self, value: &Value) -> Result<LeaseValue, HeapFault> {
        if self.local.frozen {
            return Err(invalid("lease graph is frozen"));
        }
        let Some(handle) = value.handle() else {
            return Ok(LeaseValue {
                repr: Repr::Scalar(*value),
            });
        };
        if let Some(index) = self
            .local
            .existing
            .get(&(handle.index(), handle.generation()))
            .copied()
        {
            let object = self.local.object(index);
            self.local.index(object)?;
            let slot = &self.local.slots[index as usize];
            let existing = slot.existing();
            if !existing.cold {
                let valid = if slot.kind.mutable() {
                    matches!(value, Value::ObjectRef(_))
                        || std::mem::discriminant(value)
                            == std::mem::discriminant(&slot.kind.naming(handle))
                } else {
                    existing.value.is_some_and(|entry| {
                        std::mem::discriminant(value) == std::mem::discriminant(&entry)
                    })
                };
                if !valid {
                    return Err(invalid("entry alias changes its value kind"));
                }
            }

            return Ok(LeaseValue {
                repr: Repr::Object(object, Some(*value)),
            });
        }
        let ledger = self.heap.region_ledger()?;
        self.authenticate_entry(&ledger)?;
        let dependency = Dependencies::capture(&ledger, self.arena(), &[handle])
            .ok_or_else(|| invalid("lease import has no current dependency"))?;
        self.validate_entry_value(value, &ledger)?;
        let body = self.arena().body(handle).map_err(arena_fault)?;
        let kind = match body {
            Body::List(_) => Kind::List,
            Body::Tuple(_) => Kind::Tuple,
            Body::Map(_) => Kind::Map,
            Body::Set(_) => Kind::Set,
            Body::Record(_) => Kind::Record,
            Body::Cell(_) => Kind::Cell,
            Body::Closure { .. } => Kind::Closure,
            _ => Kind::Existing,
        };
        let facts = self.arena().facts(handle).map_err(arena_fault)?;
        let entity = facts.ledger_entity();
        let cold = !self
            .arena()
            .region_body_resident(handle)
            .map_err(arena_fault)?;
        let bytes = self.arena().logical_local(value)?;
        let depth = self.arena().summary_of(value).depth;
        let (incoming, body_bytes) = if let Some(entity) = entity {
            let row = ledger
                .state
                .entities
                .get(&entity_key(entity), self.heap.pages())?
                .ok_or_else(|| invalid("imported object is not source retained"))?;
            (row.incoming, row.body.bytes)
        } else {
            (0, bytes)
        };
        let len = match body {
            Body::List(seq) | Body::Tuple(seq) => seq.len,
            Body::Map(table) | Body::Set(table) | Body::Record(table) => table.len,
            _ => 0,
        };
        let next = if !cold && matches!(kind, Kind::Map | Kind::Set | Kind::Record) {
            self.arena()
                .region_table_next(handle)
                .map_err(arena_fault)?
        } else {
            0
        };
        drop(ledger);
        self.dependencies.extend(dependency)?;
        if let Some(entity) = entity
            && let Some(index) = self.local.entities.get(&entity).copied()
        {
            self.local
                .existing
                .insert((handle.index(), handle.generation()), index);
            let slot = &mut self.local.slots[index as usize];
            slot.kind = kind;
            let existing = slot.existing_mut();
            existing.value = Some(*value);
            existing.len = len;
            existing.next = next;
            existing.cold = cold;
            capacity(&mut self.local.bindings, 1)?;
            self.local.bindings.push(Binding::Callable(index, handle));
            return Ok(LeaseValue {
                repr: Repr::Object(self.local.object(index), Some(*value)),
            });
        }
        capacity(&mut self.local.slots, 1)?;
        let index = u32::try_from(self.local.slots.len()).map_err(|_| exhausted())?;
        self.local.slots.push(ObjectSlot {
            generation: 1,
            ordinal: None,
            kind,
            body: None,
            existing: Some(Existing {
                value: Some(*value),
                entity,
                edges: Vec::new(),
                overlays: Default::default(),
                len,
                next,
                callable: None,
                cold,
            }),
            bytes: body_bytes,
            incoming,
            live: true,
            content: 0,
            structural: 0,
            draft_epoch: 0,
            draft: 0,
            retiring: false,
            measured: 0,
            depth,
        });
        self.local
            .existing
            .insert((handle.index(), handle.generation()), index);
        if let Some(entity) = entity {
            self.local.entities.insert(entity, index);
        }
        capacity(&mut self.local.bindings, 2)?;
        self.local.bindings.push(Binding::Callable(index, handle));
        self.local
            .bindings
            .push(Binding::Edges(index, entity, *value));
        Ok(LeaseValue {
            repr: Repr::Object(self.local.object(index), Some(*value)),
        })
    }
    fn validate_entry_value(&self, value: &Value, ledger: &Ledger) -> Result<(), HeapFault> {
        let mut pending = vec![*value];
        let mut seen = std::collections::BTreeSet::new();
        let check = |holding: &Holding| {
            holding.each_edge(self.heap.pages(), &mut |edge| {
                if let Identity::Object(entity) = edge.target
                    && ledger.entity_holding(entity, self.heap.pages())?.is_none()
                {
                    return Err(invalid(
                        "import references an object outside the entry graph",
                    ));
                }
                Ok(())
            })
        };
        while let Some(value) = pending.pop() {
            let Some(handle) = value.handle() else {
                continue;
            };
            if !seen.insert((handle.index(), handle.generation())) {
                continue;
            }
            let body = self.arena().body(handle).map_err(arena_fault)?;
            let valid = matches!(
                (&value, body),
                (Value::List(_), Body::List(_))
                    | (Value::Map(_), Body::Map(_))
                    | (Value::Set(_), Body::Set(_))
                    | (Value::Record(_), Body::Record(_))
                    | (Value::Tuple(_), Body::Tuple(_))
                    | (Value::Closure(_), Body::Closure { .. })
                    | (Value::LongText(_), Body::LongText(_))
                    | (Value::BigInt(_), Body::BigInt(_))
                    | (Value::Bytes(_), Body::Bytes(_))
                    | (Value::BigTimestamp(_), Body::Timestamp(_))
                    | (Value::Error(_), Body::Error(_))
                    | (Value::Function(_), Body::Function(_))
                    | (Value::Handle(_), Body::Handle(_))
            ) || matches!(value, Value::ObjectRef(_))
                && crate::arena::region_mutable_body(body);
            let facts = self.arena().facts(handle).map_err(arena_fault)?;
            if !facts.cold && !valid {
                return Err(invalid("entry value names a different body kind"));
            }
            if matches!(
                value,
                Value::List(_)
                    | Value::Map(_)
                    | Value::Set(_)
                    | Value::Record(_)
                    | Value::Closure(_)
                    | Value::ObjectRef(_)
            ) {
                let entity = facts
                    .ledger_entity()
                    .ok_or_else(|| invalid("mutable import has no source identity"))?;
                if ledger.entity_holding(entity, self.heap.pages())?.is_none() {
                    return Err(invalid("mutable import is not source retained"));
                }
            } else if matches!(value, Value::Tuple(_) | Value::Error(_)) {
                if let Some(holding) = facts.logical_holding.get() {
                    check(holding)?;
                } else if let Some(page) = facts.page.get() {
                    check(&Holding::from_page(self.heap.pages(), page)?)?;
                } else {
                    crate::arena::each_member(body, &mut |value| pending.push(*value));
                }
            }
        }
        Ok(())
    }
    pub(super) fn bind_entity(&mut self, entity: EntityId) -> Result<LeaseObject, HeapFault> {
        if let Some(index) = self.local.entities.get(&entity).copied() {
            return Ok(self.local.object(index));
        }
        if let Some(handle) = self.arena().logical_handle(entity) {
            return self
                .import_one(&Value::ObjectRef(handle))
                .and_then(|value| self.object(value));
        }
        let ledger = self.heap.region_ledger()?;
        self.authenticate_entry(&ledger)?;
        let row = ledger
            .state
            .entities
            .get(&entity_key(entity), self.heap.pages())?
            .ok_or_else(|| invalid("lease edge target is absent"))?;
        drop(ledger);
        capacity(&mut self.local.slots, 1)?;
        let index = u32::try_from(self.local.slots.len()).map_err(|_| exhausted())?;
        self.local.slots.push(ObjectSlot {
            generation: 1,
            ordinal: None,
            kind: Kind::Existing,
            body: None,
            existing: Some(Existing {
                value: None,
                entity: Some(entity),
                edges: Vec::new(),
                overlays: Default::default(),
                len: 0,
                next: 0,
                callable: None,
                cold: true,
            }),
            bytes: row.body.bytes,
            incoming: row.incoming,
            live: true,
            content: 0,
            structural: 0,
            draft_epoch: 0,
            draft: 0,
            retiring: false,
            measured: 0,
            depth: 0,
        });
        self.local.entities.insert(entity, index);
        capacity(&mut self.local.bindings, 1)?;
        self.local
            .bindings
            .push(Binding::Edges(index, Some(entity), Value::Null));
        Ok(self.local.object(index))
    }
    pub(super) fn bind_edges(
        &mut self,
        index: u32,
        entity: Option<EntityId>,
        value: Value,
    ) -> Result<(), HeapFault> {
        let holding = if let Some(entity) = entity {
            self.heap
                .region_ledger()?
                .entity_holding(entity, self.heap.pages())?
                .ok_or_else(|| invalid("lease body is absent"))?
        } else {
            let pages = self.heap.pages().clone();
            match &mut self.storage {
                LeaseStorage::Arena(arena) => Holding::from_native(arena.write(), &pages, &value)?,
                _ => return Err(invalid("lease already materialized")),
            }
        };
        let mut edges = Vec::new();
        holding.each_edge(self.heap.pages(), &mut |edge| {
            if let Identity::Object(entity) = edge.target {
                capacity(&mut edges, 1)?;
                edges.push((entity, edge.count));
            }
            Ok(())
        })?;
        for (target, count) in edges {
            let object = self.bind_entity(target)?;
            self.local.slots[index as usize]
                .existing_mut()
                .edges
                .push((object, count));
        }
        Ok(())
    }
    pub fn scalar(&self, value: LeaseValue) -> Result<Value, HeapFault> {
        match value.repr {
            Repr::Scalar(value) => Ok(value),
            _ => Err(invalid("lease object is not an inline scalar")),
        }
    }
    pub fn object(&self, value: LeaseValue) -> Result<LeaseObject, HeapFault> {
        match value.repr {
            Repr::Object(object, _) => {
                self.local.index(object)?;
                Ok(object)
            }
            _ => Err(invalid("lease scalar is not an object")),
        }
    }
    /// Canonical logical bytes of one current source occurrence.
    pub fn price(&self, value: LeaseValue) -> Result<u64, HeapFault> {
        if self.local.frozen {
            return Err(invalid("lease graph is frozen"));
        }
        match value.repr {
            Repr::Scalar(value) => self.arena().logical_local(&value),
            Repr::Object(object, _) => {
                let slot = &self.local.slots[self.local.index(object)?];
                if slot
                    .existing
                    .as_ref()
                    .is_some_and(|existing| existing.entity.is_some())
                    || slot.kind.mutable()
                {
                    Ok(Cost::REFERENCE)
                } else {
                    Ok(slot.bytes)
                }
            }
        }
    }
    pub(super) fn key(&self, value: LeaseValue) -> Result<LeaseKey, LeaseFault> {
        match value.repr {
            Repr::Scalar(value) => self
                .arena()
                .key(&value)
                .map(LeaseKey::Canonical)
                .map_err(local_native),
            Repr::Object(object, native) => {
                let slot = &self.local.slots[self.local.index(object)?];
                if let Some(value) = native {
                    return self
                        .arena()
                        .key(&value)
                        .map(LeaseKey::Canonical)
                        .map_err(local_native);
                }
                match &slot.body {
                    Some(LocalBody::Text(text)) => Ok(LeaseKey::Canonical(Key::of_text(text))),
                    Some(LocalBody::Cell(_)) => Ok(LeaseKey::Local(object)),
                    Some(LocalBody::Bytes(_) | LocalBody::Values(_)) => {
                        let mut out = lash_kernel_value::tokens::TokenWriter::new();
                        if !self.key_tokens(value, &mut out)? {
                            if let Some(LocalBody::Values(values)) = &slot.body
                                && slot.kind == Kind::Tuple
                            {
                                let mut keys = Vec::new();
                                capacity(&mut keys, values.len())?;
                                for value in values {
                                    keys.push(self.key(*value)?);
                                }
                                return Ok(LeaseKey::Composite(keys));
                            }
                            return Err(crate::arena::invalid_key().into());
                        }
                        let bytes = out.into_bytes();
                        let normal = if bytes.len() <= lash_kernel_doc::heap::NormalKey::INLINE_MAX
                        {
                            lash_kernel_doc::heap::NormalKey::Inline(bytes)
                        } else {
                            lash_kernel_doc::heap::NormalKey::Digest(
                                lash_kernel_doc::heap_values::key_digest(&bytes),
                            )
                        };
                        Ok(LeaseKey::Canonical(Key::Normal(Box::new(normal))))
                    }
                    _ => Err(crate::arena::invalid_key().into()),
                }
            }
        }
    }
    pub(super) fn key_tokens(
        &self,
        value: LeaseValue,
        out: &mut lash_kernel_value::tokens::TokenWriter,
    ) -> Result<bool, LeaseFault> {
        match value.repr {
            Repr::Scalar(value) => self
                .arena()
                .region_key_tokens(&value, out)
                .map_err(local_native),
            Repr::Object(object, native) => {
                self.local.index(object)?;
                if let Some(native) = native {
                    return self
                        .arena()
                        .region_key_tokens(&native, out)
                        .map_err(local_native);
                }
                let slot = &self.local.slots[object.slot as usize];
                match &slot.body {
                    Some(LocalBody::Text(text)) => {
                        out.sum(4, 1);
                        out.text(text);
                        Ok(true)
                    }
                    Some(LocalBody::Bytes(bytes)) => {
                        out.sum(5, 1);
                        out.bytes(bytes);
                        Ok(true)
                    }
                    Some(LocalBody::Values(values)) if slot.kind == Kind::Tuple => {
                        out.sum(9, 1);
                        out.seq(values.len() as u64);
                        for value in values {
                            if !self.key_tokens(*value, out)? {
                                return Ok(false);
                            }
                        }
                        Ok(true)
                    }
                    _ => Ok(false),
                }
            }
        }
    }
    pub(super) fn spelling_price(&self, key: &Spelling, record: bool) -> Result<u64, HeapFault> {
        let bytes = match key {
            Spelling::Value(value) => self.price(*value)?,
            Spelling::Field(name) => plus(Cost::REFERENCE, count(name.len())?)?,
        };
        if record {
            bytes
                .checked_sub(Cost::REFERENCE)
                .ok_or_else(|| invalid("record key cost underflow"))
        } else {
            Ok(bytes)
        }
    }
    pub(super) fn entry_price(&self, entry: &Entry, record: bool) -> Result<u64, HeapFault> {
        plus(
            self.spelling_price(&entry.key, record)?,
            self.price(entry.value)?,
        )
    }
    pub(super) fn address(&self, member: &LeaseMember) -> Result<Address, LeaseFault> {
        Ok(match member {
            LeaseMember::List(index) => Address::List(*index),
            LeaseMember::Table(value) => Address::Table(self.key(*value)?),
            LeaseMember::Field(name) => Address::Table(LeaseKey::Canonical(Key::of_text(name))),
            LeaseMember::Variable => Address::Variable,
        })
    }
    pub(super) fn member_entry(
        &mut self,
        index: usize,
        member: &LeaseMember,
        address: &Address,
    ) -> Result<Option<Entry>, LeaseFault> {
        let slot = &self.local.slots[index];
        if slot.existing.as_ref().is_some_and(|existing| existing.cold) {
            return Err(LeaseFault::BoundaryRequired);
        }
        if matches!(member, LeaseMember::Field(_)) && slot.kind != Kind::Record {
            return Err(NativeError::Decline.into());
        }
        if let Some(existing) = &slot.existing {
            if let Some(overlay) = existing.overlays.get(address) {
                return Ok(overlay.current.clone());
            }
            let value = existing.value.ok_or(LeaseFault::BoundaryRequired)?;
            let handle = value.handle().ok_or(LeaseFault::BoundaryRequired)?;
            let (key, value, sequence) = match (slot.kind, member, address) {
                (Kind::List | Kind::Tuple, LeaseMember::List(at), _) => {
                    if *at >= existing.len {
                        return Ok(None);
                    }
                    let value = self
                        .arena()
                        .item(handle, *at)
                        .map_err(local_native)?
                        .ok_or(LeaseFault::BoundaryRequired)?;
                    (None, value, *at)
                }
                (Kind::Cell, LeaseMember::Variable, _) => {
                    (None, self.arena().cell(handle).map_err(local_native)?, 0)
                }
                (Kind::Map | Kind::Set | Kind::Record, _, Address::Table(key)) => {
                    let LeaseKey::Canonical(key) = key else {
                        return Ok(None);
                    };
                    let Some((old, value, sequence)) = self
                        .arena()
                        .region_table_entry(handle, key)
                        .map_err(local_native)?
                    else {
                        return Ok(None);
                    };
                    (Some(old), value, sequence)
                }
                _ => return Err(NativeError::Decline.into()),
            };
            let value = self.import(&value)?;
            let key = match key {
                Some(key) => Spelling::Value(self.import(&key)?),
                None => Spelling::Value(LeaseValue {
                    repr: Repr::Scalar(Value::Absent),
                }),
            };
            return Ok(Some(Entry {
                key,
                value,
                sequence,
            }));
        }
        Ok(match (&slot.body, member, address) {
            (Some(LocalBody::Values(values)), LeaseMember::List(at), _) => values
                .get(usize::try_from(*at).map_err(|_| invalid("lease index overflow"))?)
                .copied()
                .map(|value| Entry {
                    key: Spelling::Value(value),
                    value,
                    sequence: *at,
                }),
            (Some(LocalBody::Cell(value)), LeaseMember::Variable, _) => Some(Entry {
                key: Spelling::Value(*value),
                value: *value,
                sequence: 0,
            }),
            (Some(LocalBody::Table(table)), _, Address::Table(key)) => {
                table.index.get(key).map(|at| table.entries[*at].clone())
            }
            _ => return Err(NativeError::Decline.into()),
        })
    }
    pub fn read(
        &mut self,
        object: LeaseObject,
        member: &LeaseMember,
    ) -> Result<LeaseValue, LeaseFault> {
        if self.local.frozen {
            return Err(invalid("lease graph is frozen").into());
        }
        let index = self.local.index(object)?;
        let address = self.address(member)?;
        Ok(self.member_entry(index, member, &address)?.map_or(
            LeaseValue {
                repr: Repr::Scalar(Value::Absent),
            },
            |entry| entry.value,
        ))
    }
}
