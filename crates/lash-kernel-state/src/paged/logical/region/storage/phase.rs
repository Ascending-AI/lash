use super::*;

impl<'s> StateLease<'s> {
    /// Operands remain borrowed until reserve, so staging cannot allocate the
    /// requested body before the caller checks the exact source affordability.
    pub fn stage<'a>(&'a mut self, op: LeaseOp<'a>) -> Result<LeaseStep<'a, 's>, LeaseFault> {
        self.local.begin()?;
        let mut candidate = Candidate {
            op,
            member: None,
            allocation: None,
            result: None,
        };
        let mut facts = LeaseFacts::default();
        if let LeaseOp::Adopt { .. } = &candidate.op {
            // The adopted bodies are retained, and hold what they name, in
            // the phase that transfers the answer; an adopted slot nothing
            // holds after it leaves in the same phase.
            let adopted = self
                .local
                .adopted
                .clone()
                .ok_or_else(|| invalid("no native answer is adopted"))?;
            self.local
                .ordinal
                .checked_add(count(adopted.len())?)
                .ok_or_else(|| invalid("lease allocation ordinal overflow"))?;
            for index in adopted {
                self.local.change(self.local.object(index), 0)?;
                let slot = &self.local.slots[index as usize];
                if slot.kind.mutable() {
                    facts.delta = facts
                        .delta
                        .checked_add(i128::from(slot.bytes))
                        .ok_or_else(|| invalid("adopted body cost overflow"))?;
                    self.body_edges(index as usize, 1)?;
                }
            }
        }
        match &candidate.op {
            LeaseOp::Transfer { holds, releases }
            | LeaseOp::Adopt { holds, releases }
            | LeaseOp::FrameTransfer {
                holds, releases, ..
            } => {
                for (values, sign) in [(*holds, 1_i128), (*releases, -1_i128)] {
                    for &(value, capture) in values {
                        if capture {
                            let object = self.object(value)?;
                            let index = self.local.index(object)?;
                            self.load_head(index)?;
                            if self.local.slots[index].kind != Kind::Cell {
                                return Err(
                                    invalid("a capture phase must name a variable cell").into()
                                );
                            }
                            self.local.change(object, sign)?;
                        } else {
                            facts.delta = facts
                                .delta
                                .checked_add(i128::from(self.price(value)?) * sign)
                                .ok_or_else(|| invalid("lease root transfer overflow"))?;
                            self.value_edges(value, sign)?;
                        }
                    }
                }
            }
            LeaseOp::Allocate(allocation) | LeaseOp::AllocateMove { allocation, .. } => {
                if matches!(candidate.op, LeaseOp::AllocateMove { .. })
                    && !matches!(allocation.body, Allocation::Tuple(_) | Allocation::Cell(_))
                {
                    return Err(
                        invalid("atomic source allocation must construct a tuple or cell").into(),
                    );
                }
                let (kind, bytes, members, payload) = self.allocation_facts(&allocation.body)?;
                facts.members = members;
                facts.payload = payload;
                if let Allocation::Tuple(values) = &allocation.body {
                    let mut depth = 0;
                    for value in *values {
                        if let Repr::Object(object, _) = value.repr {
                            let index = self.local.index(object)?;
                            depth = depth.max(self.local.slots[index].depth);
                        }
                    }
                    facts.depth = depth
                        .checked_add(1)
                        .ok_or_else(|| invalid("immutable nesting depth overflow"))?;
                }
                if let Allocation::Text(text) = allocation.body
                    && let Some(text) = ShortText::new(text)
                {
                    let value = LeaseValue {
                        repr: Repr::Scalar(Value::Text(text)),
                    };
                    facts.delta = i128::from(self.price(value)?);
                    candidate.result = Some(value);
                } else {
                    let object = self.local.empty_slot()?;
                    self.local
                        .ordinal
                        .checked_add(u64::from(kind.mutable()))
                        .ok_or_else(|| invalid("lease allocation ordinal overflow"))?;
                    candidate.result = Some(LeaseValue {
                        repr: Repr::Object(object, None),
                    });
                    candidate.allocation = Some((kind, bytes, members));
                    facts.delta = i128::from(bytes)
                        + if kind.mutable() {
                            i128::from(Cost::REFERENCE)
                        } else {
                            0
                        };
                    self.allocation_edges(&allocation.body)?;
                }
                if let LeaseOp::AllocateMove { releases, .. } = &candidate.op {
                    for &value in *releases {
                        facts.delta = facts
                            .delta
                            .checked_sub(i128::from(self.price(value)?))
                            .ok_or_else(|| invalid("allocation source move overflow"))?;
                        self.value_edges(value, -1)?;
                    }
                }
            }
            LeaseOp::HoldCapture { cell, count } | LeaseOp::ReleaseCapture { cell, count } => {
                let index = self.local.index(*cell)?;
                self.load_head(index)?;
                if self.local.slots[index].kind != Kind::Cell {
                    return Err(invalid("a capture phase must name a variable cell").into());
                }
                let count = i128::from(*count)
                    * if matches!(candidate.op, LeaseOp::HoldCapture { .. }) {
                        1
                    } else {
                        -1
                    };
                self.local.change(*cell, count)?;
            }
            LeaseOp::Hold { value, count } => {
                facts.delta = i128::from(self.price(*value)?)
                    .checked_mul(i128::from(*count))
                    .ok_or_else(|| invalid("lease hold overflow"))?;
                self.value_edges(*value, i128::from(*count))?;
            }
            LeaseOp::Release { value, count } => {
                facts.delta = -i128::from(self.price(*value)?)
                    .checked_mul(i128::from(*count))
                    .ok_or_else(|| invalid("lease release overflow"))?;
                self.value_edges(*value, -i128::from(*count))?;
            }
            LeaseOp::Grow { object, additional } => {
                let slot = &self.local.slots[self.local.index(*object)?];
                let size = usize::try_from(*additional).map_err(|_| exhausted())?;
                if slot.existing.as_ref().is_some_and(|existing| existing.cold) {
                    return Err(LeaseFault::BoundaryRequired);
                }
                facts.members = *additional;
                if slot.existing.is_some() && slot.kind.editable() {
                    self.length(self.local.index(*object)?)
                        .checked_add(*additional)
                        .ok_or_else(|| invalid("lease growth length overflow"))?;
                } else {
                    match &slot.body {
                        Some(LocalBody::Values(values)) => {
                            values.len().checked_add(size).ok_or_else(exhausted)?;
                        }
                        Some(LocalBody::Table(table)) => {
                            table
                                .entries
                                .len()
                                .checked_add(size)
                                .ok_or_else(exhausted)?;
                        }
                        _ => return Err(LeaseFault::BoundaryRequired),
                    }
                }
            }
            LeaseOp::Member {
                object,
                member,
                next,
            } => {
                let index = self.local.index(*object)?;
                self.load_head(index)?;
                let kind = self.local.slots[index].kind;
                if !kind.editable() {
                    return Err(NativeError::Decline.into());
                }
                if kind == Kind::Record
                    && let LeaseMember::Table(key) = member
                    && !self.is_text(*key)?
                {
                    return Err(NativeError::Decline.into());
                }
                if kind == Kind::Set
                    && next.is_some_and(|value| !matches!(value.repr, Repr::Scalar(Value::Absent)))
                {
                    return Err(NativeError::Decline.into());
                }
                let address = self.address(member)?;
                let old = self.member_entry(index, member, &address)?;
                if kind == Kind::Cell && next.is_none() {
                    return Err(NativeError::Decline.into());
                }
                let len = self.length(index);
                if let LeaseMember::List(at) = member
                    && (*at > len || *at == len && next.is_none())
                {
                    return Err(NativeError::Decline.into());
                }
                // List removal moves its suffix at the source's structural phase.
                let structural = old.is_some() != next.is_some();
                let new = if let Some(value) = next {
                    self.local.value_index(*value)?;
                    let key = match &old {
                        Some(entry) => entry.key.clone(),
                        None => match member {
                            LeaseMember::Table(key) => Spelling::Value(*key),
                            LeaseMember::Field(name) => Spelling::Field(name.clone()),
                            _ => Spelling::Value(*value),
                        },
                    };
                    let sequence = match &old {
                        Some(entry) => entry.sequence,
                        None => {
                            if let LeaseMember::List(at) = member {
                                *at
                            } else {
                                self.next_sequence(index)
                            }
                        }
                    };
                    Some(Entry {
                        key,
                        value: *value,
                        sequence,
                    })
                } else {
                    None
                };
                let record = kind == Kind::Record;
                let table = matches!(kind, Kind::Map | Kind::Set | Kind::Record);
                let old_bytes = old
                    .as_ref()
                    .map(|entry| {
                        if table {
                            self.entry_price(entry, record)
                        } else {
                            self.price(entry.value)
                        }
                    })
                    .transpose()?
                    .unwrap_or(0);
                let new_bytes = new
                    .as_ref()
                    .map(|entry| {
                        if table {
                            self.entry_price(entry, record)
                        } else {
                            self.price(entry.value)
                        }
                    })
                    .transpose()?
                    .unwrap_or(0);
                facts.delta = i128::from(new_bytes) - i128::from(old_bytes);
                facts.members = u64::from(structural);
                facts.payload = new_bytes;
                if let Some(entry) = &new {
                    self.entry_edges(entry, table, 1)?;
                }
                if let Some(entry) = &old {
                    self.entry_edges(entry, table, -1)?;
                }
                self.local.slots[index]
                    .bytes
                    .checked_sub(old_bytes)
                    .and_then(|bytes| bytes.checked_add(new_bytes))
                    .ok_or_else(|| invalid("lease body cost overflow"))?;
                self.local.slots[index]
                    .content
                    .checked_add(1)
                    .ok_or_else(|| invalid("lease content revision overflow"))?;
                self.local.slots[index]
                    .structural
                    .checked_add(u64::from(structural))
                    .ok_or_else(|| invalid("lease structural revision overflow"))?;
                if structural && new.is_some() {
                    self.next_sequence(index)
                        .checked_add(1)
                        .ok_or_else(|| invalid("lease sequence overflow"))?;
                }
                let body_bytes = self.local.slots[index].bytes - old_bytes + new_bytes;
                candidate.member = Some(MemberPlan {
                    address,
                    old,
                    next: new,
                    structural,
                    body_bytes,
                });
            }
        }
        self.finish_releases(&candidate, &mut facts)?;
        facts.peak =
            u64::try_from(facts.delta.max(0)).map_err(|_| invalid("lease phase peak overflow"))?;
        if let LeaseOp::FrameTransfer { entering, .. } = candidate.op {
            let header = i128::from(Cost::FRAME);
            facts.delta = facts
                .delta
                .checked_add(if entering { header } else { -header })
                .ok_or_else(|| invalid("lease frame transfer overflow"))?;
            if entering {
                facts.peak = facts
                    .peak
                    .checked_add(Cost::FRAME)
                    .ok_or_else(|| invalid("lease frame peak overflow"))?;
            }
        }
        self.local
            .delta
            .checked_add(facts.delta)
            .ok_or_else(|| invalid("lease prefix delta overflow"))?;
        self.local
            .version
            .checked_add(1)
            .ok_or_else(|| invalid("lease graph version overflow"))?;
        Ok(LeaseStep {
            lease: self,
            candidate,
            facts,
        })
    }
    pub(super) fn is_text(&self, value: LeaseValue) -> Result<bool, HeapFault> {
        Ok(match value.repr {
            Repr::Scalar(Value::Text(_)) => true,
            Repr::Object(object, _) => {
                let slot = &self.local.slots[self.local.index(object)?];
                slot.kind == Kind::Text
                    || slot
                        .existing
                        .as_ref()
                        .and_then(|e| e.value)
                        .is_some_and(|v| matches!(v, Value::LongText(_)))
            }
            _ => false,
        })
    }
    pub(super) fn length(&self, index: usize) -> u64 {
        let slot = &self.local.slots[index];
        if let Some(existing) = &slot.existing {
            existing.len
        } else {
            match &slot.body {
                Some(LocalBody::Values(values)) => values.len() as u64,
                Some(LocalBody::Table(table)) => table.entries.len() as u64,
                _ => 0,
            }
        }
    }
    pub(super) fn next_sequence(&self, index: usize) -> u64 {
        let slot = &self.local.slots[index];
        if let Some(existing) = &slot.existing {
            existing.next
        } else {
            match &slot.body {
                Some(LocalBody::Table(table)) => table.next,
                _ => self.length(index),
            }
        }
    }
    pub(super) fn allocation_facts(
        &mut self,
        body: &Allocation<'_>,
    ) -> Result<(Kind, u64, u64, u64), LeaseFault> {
        let mut bytes = 0;
        let mut members = 0;
        let mut payload = 0;
        let kind = match body {
            Allocation::List(values) | Allocation::Tuple(values) => {
                members = count(values.len())?;
                for value in *values {
                    bytes = plus(bytes, self.price(*value)?)?;
                }
                if matches!(body, Allocation::List(_)) {
                    Kind::List
                } else {
                    Kind::Tuple
                }
            }
            Allocation::Map(_)
            | Allocation::Record(_)
            | Allocation::RecordFields { .. }
            | Allocation::Set(_) => {
                let record = matches!(
                    body,
                    Allocation::Record(_) | Allocation::RecordFields { .. }
                );
                self.normalize_table_inputs(body)?;
                members = count(self.local.table_inputs.rows.len())?;
                for row in 0..self.local.table_inputs.rows.len() {
                    bytes = plus(
                        bytes,
                        self.entry_price(&self.normalized_entry(body, row), record)?,
                    )?;
                }
                if record {
                    Kind::Record
                } else if matches!(body, Allocation::Set(_)) {
                    Kind::Set
                } else {
                    Kind::Map
                }
            }
            Allocation::Closure { identity, captures } => {
                if identity.capture_names.len() != captures.len() {
                    return Err(
                        invalid("closure capture shape differs from its actual identity").into(),
                    );
                }
                members = count(captures.len())?;
                for capture in *captures {
                    let index = self.local.index(self.object(*capture)?)?;
                    if self.local.slots[index].kind != Kind::Cell {
                        return Err(invalid("closure capture is not a variable cell").into());
                    }
                }
                bytes = members
                    .checked_mul(Cost::ENTITY_EDGE)
                    .ok_or_else(|| invalid("closure capture cost overflow"))?;
                Kind::Closure
            }
            Allocation::Cell(value) => {
                members = 1;
                bytes = self.price(**value)?;
                Kind::Cell
            }
            Allocation::Text(text) => {
                payload = count(text.len())?;
                bytes = payload;
                Kind::Text
            }
            Allocation::Bytes(value) => {
                payload = count(value.len())?;
                bytes = payload;
                Kind::Bytes
            }
            // Leaves are priced as the arena and adoption price them.
            Allocation::BigInt(integer) => {
                payload = integer.bits().div_ceil(8);
                bytes = payload;
                Kind::Leaf(Leaf::BigInt)
            }
            Allocation::Function(_) => {
                bytes = 64;
                Kind::Leaf(Leaf::Function)
            }
        };
        bytes = plus(
            if kind.mutable() {
                Cost::OBJECT
            } else {
                Cost::REFERENCE
            },
            bytes,
        )?;
        Ok((kind, bytes, members, payload))
    }
    pub(super) fn allocation_edges(&mut self, body: &Allocation<'_>) -> Result<(), HeapFault> {
        match body {
            Allocation::List(values) | Allocation::Tuple(values) => {
                for value in *values {
                    self.value_edges(*value, 1)?;
                }
            }
            Allocation::Map(_)
            | Allocation::Record(_)
            | Allocation::RecordFields { .. }
            | Allocation::Set(_) => {
                for row in 0..self.local.table_inputs.rows.len() {
                    self.entry_edges(&self.normalized_entry(body, row), true, 1)?;
                }
            }
            Allocation::Cell(value) => self.value_edges(**value, 1)?,
            Allocation::Closure { captures, .. } => {
                for capture in *captures {
                    self.value_edges(*capture, 1)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) fn entry_edges(
        &mut self,
        entry: &Entry,
        table: bool,
        delta: i128,
    ) -> Result<(), HeapFault> {
        if table && let Spelling::Value(key) = entry.key {
            self.value_edges(key, delta)?;
        }
        self.value_edges(entry.value, delta)
    }
    pub(super) fn value_edges(&mut self, value: LeaseValue, delta: i128) -> Result<(), HeapFault> {
        let Some(index) = self.local.value_index(value)? else {
            return Ok(());
        };
        let object = self.local.object(index as u32);
        let slot = &self.local.slots[index];
        if slot
            .existing
            .as_ref()
            .is_some_and(|existing| existing.entity.is_some())
            || slot.kind.mutable()
        {
            return self.local.change(object, delta);
        }
        if slot.existing.is_none() {
            self.local.change(object, delta)?;
        }
        if let Some(existing) = &self.local.slots[index].existing {
            for at in 0..existing.edges.len() {
                let (target, count) = self.local.slots[index].existing().edges[at];
                self.local.change(
                    target,
                    delta
                        .checked_mul(i128::from(count))
                        .ok_or_else(|| invalid("lease edge overflow"))?,
                )?;
            }
        } else if let Some(LocalBody::Values(values)) = &self.local.slots[index].body {
            let len = values.len();
            for at in 0..len {
                let value = match &self.local.slots[index].body {
                    Some(LocalBody::Values(values)) => values[at],
                    _ => unreachable!(),
                };
                self.value_edges(value, delta)?;
            }
        }
        Ok(())
    }
    pub(super) fn finish_releases(
        &mut self,
        candidate: &Candidate<'_>,
        facts: &mut LeaseFacts,
    ) -> Result<(), HeapFault> {
        while let Some(index) = self.local.zero.pop() {
            #[cfg(any(test, feature = "perf-witness"))]
            {
                self.local.release_visits += 1;
            }
            let index = index as usize;
            if self.local.slots[index].retiring || self.local.slots[index].draft != 0 {
                continue;
            }
            self.local.slots[index].retiring = true;
            let slot = &self.local.slots[index];
            if slot.kind.mutable()
                || slot
                    .existing
                    .as_ref()
                    .is_some_and(|existing| existing.entity.is_some())
            {
                let replacement = match &candidate.op {
                    LeaseOp::Member { object, .. } if object.slot as usize == index => {
                        candidate.member.as_ref()
                    }
                    _ => None,
                };
                let bytes = replacement.map_or(slot.bytes, |plan| plan.body_bytes);
                facts.delta -= i128::from(bytes);
                facts.released = plus(facts.released, bytes)?;
                self.body_edges(index, -1)?;
                if let Some(plan) = replacement {
                    let table = matches!(plan.address, Address::Table(_));
                    if let Some(old) = &plan.old {
                        self.entry_edges(old, table, 1)?;
                    }
                    if let Some(new) = &plan.next {
                        self.entry_edges(new, table, -1)?;
                    }
                }
            }
        }
        for index in &self.local.touched {
            #[cfg(any(test, feature = "perf-witness"))]
            {
                self.local.release_visits += 1;
            }
            let slot = &self.local.slots[*index as usize];
            if slot.draft < 0 || slot.draft > i128::from(u64::MAX) {
                return Err(invalid("lease source multiplicity underflow or overflow"));
            }
        }
        Ok(())
    }
    pub(super) fn body_edges(&mut self, index: usize, delta: i128) -> Result<(), HeapFault> {
        if let Some(existing) = &self.local.slots[index].existing {
            for at in 0..existing.edges.len() {
                let (target, count) = self.local.slots[index].existing().edges[at];
                self.local.change(target, delta * i128::from(count))?;
            }
            // Edits retain their original member only once, without a write tape.
            let addresses: Vec<_> = self.local.slots[index]
                .existing()
                .overlays
                .keys()
                .cloned()
                .collect();
            for address in addresses {
                let overlay = &self.local.slots[index].existing().overlays[&address];
                let old = overlay.original.clone();
                let new = overlay.current.clone();
                let table = matches!(address, Address::Table(_));
                if let Some(old) = old {
                    self.entry_edges(&old, table, -delta)?;
                }
                if let Some(new) = new {
                    self.entry_edges(&new, table, delta)?;
                }
            }
        } else {
            let len = match &self.local.slots[index].body {
                Some(LocalBody::Values(values)) => values.len(),
                Some(LocalBody::Table(table)) => table.entries.len(),
                Some(LocalBody::Cell(_)) => 1,
                Some(LocalBody::Closure(closure)) => closure.captures.len(),
                _ => 0,
            };
            for at in 0..len {
                let (entry, table) = match &self.local.slots[index].body {
                    Some(LocalBody::Values(values)) => (
                        Entry {
                            key: Spelling::Value(values[at]),
                            value: values[at],
                            sequence: at as u64,
                        },
                        false,
                    ),
                    Some(LocalBody::Table(table)) => (table.entries[at].clone(), true),
                    Some(LocalBody::Closure(closure)) => (
                        Entry {
                            key: Spelling::Value(closure.captures[at]),
                            value: closure.captures[at],
                            sequence: at as u64,
                        },
                        false,
                    ),
                    Some(LocalBody::Cell(value)) => (
                        Entry {
                            key: Spelling::Value(*value),
                            value: *value,
                            sequence: 0,
                        },
                        false,
                    ),
                    _ => unreachable!(),
                };
                self.entry_edges(&entry, table, delta)?;
            }
        }
        Ok(())
    }
}
