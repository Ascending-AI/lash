use super::*;

impl<'a, 's> LeaseStep<'a, 's> {
    pub fn facts(&self) -> &LeaseFacts {
        &self.facts
    }
    pub fn reserve(self) -> Result<ReadyLeaseStep<'a, 's>, HeapFault> {
        capacity(&mut self.lease.local.free, self.lease.local.touched.len())?;
        let mut list = Vec::new();
        let body = match &self.candidate.op {
            LeaseOp::Allocate(allocation) if self.candidate.allocation.is_some() => {
                let Some(LeaseValue {
                    repr: Repr::Object(object, _),
                }) = self.candidate.result
                else {
                    return Err(invalid("allocation phase omitted its local result"));
                };
                if self.lease.local.free.is_empty() {
                    capacity(&mut self.lease.local.slots, 1)?;
                    capacity(&mut self.lease.local.free, 1)?;
                    self.lease.local.slots.push(ObjectSlot {
                        generation: object.generation,
                        ordinal: None,
                        kind: Kind::Existing,
                        body: None,
                        existing: None,
                        bytes: 0,
                        incoming: 0,
                        live: false,
                        content: 0,
                        structural: 0,
                        draft_epoch: 0,
                        draft: 0,
                        retiring: false,
                        measured: 0,
                        depth: 0,
                    });
                    self.lease.local.free.push(object.slot);
                } else {
                    self.lease.local.slots[object.slot as usize].generation = object.generation;
                }
                let reusable = self.lease.local.slots[object.slot as usize].body.take();
                Some(self.lease.reserve_allocation(&allocation.body, reusable)?)
            }
            LeaseOp::Member { object, .. } => {
                let index = self.lease.local.index(*object)?;
                let plan = self
                    .candidate
                    .member
                    .as_ref()
                    .ok_or_else(|| invalid("member phase omitted its patch"))?;
                if let Address::List(at) = plan.address
                    && plan.old.is_some()
                    && plan.next.is_none()
                    && self.lease.local.slots[index].existing.is_some()
                {
                    let len = self.lease.length(index);
                    capacity(
                        &mut list,
                        usize::try_from(len - at).map_err(|_| exhausted())?,
                    )?;
                    for position in at..len {
                        let old = self
                            .lease
                            .member_entry(
                                index,
                                &LeaseMember::List(position),
                                &Address::List(position),
                            )
                            .map_err(|error| invalid(&error.to_string()))?;
                        let next = if position + 1 < len {
                            self.lease
                                .member_entry(
                                    index,
                                    &LeaseMember::List(position + 1),
                                    &Address::List(position + 1),
                                )
                                .map_err(|error| invalid(&error.to_string()))?
                        } else {
                            None
                        };
                        list.push((position, old, next));
                    }
                }
                if let Some(existing) = &mut self.lease.local.slots[index].existing {
                    let additional =
                        list.len() + usize::from(!existing.overlays.contains_key(&plan.address));
                    existing.overlays.reserve(&plan.address, additional)?;
                } else {
                    match &mut self.lease.local.slots[index].body {
                        Some(LocalBody::Values(values))
                            if plan.old.is_none() && plan.next.is_some() =>
                        {
                            capacity(values, 1)?
                        }
                        Some(LocalBody::Table(table))
                            if plan.old.is_none() && plan.next.is_some() =>
                        {
                            capacity(&mut table.entries, 1)?;
                            table.index.try_reserve(1).map_err(|_| exhausted())?;
                        }
                        _ => {}
                    }
                }
                None
            }
            LeaseOp::Grow { object, additional } => {
                let index = self.lease.local.index(*object)?;
                let additional = usize::try_from(*additional).map_err(|_| exhausted())?;
                let kind = self.lease.local.slots[index].kind;
                if let Some(existing) = &mut self.lease.local.slots[index].existing {
                    let address = if matches!(kind, Kind::Map | Kind::Set | Kind::Record) {
                        Address::Table(LeaseKey::Canonical(Key::Int(0)))
                    } else {
                        Address::Variable
                    };
                    existing.overlays.reserve(&address, additional)?;
                } else {
                    match &mut self.lease.local.slots[index].body {
                        Some(LocalBody::Values(values)) => capacity(values, additional)?,
                        Some(LocalBody::Table(table)) => {
                            capacity(&mut table.entries, additional)?;
                            table
                                .index
                                .try_reserve(additional)
                                .map_err(|_| exhausted())?;
                        }
                        _ => unreachable!("staged growth kind"),
                    }
                }
                None
            }
            _ => None,
        };
        let result = self.candidate.result;
        Ok(ReadyLeaseStep {
            step: self,
            body,
            result,
            list,
        })
    }
}
impl ReadyLeaseStep<'_, '_> {
    pub fn result(&self) -> Option<LeaseValue> {
        self.result
    }
    pub fn accept(self) -> Option<LeaseValue> {
        let Self {
            step,
            body,
            result,
            list,
        } = self;
        let LeaseStep {
            lease,
            candidate,
            facts,
        } = step;
        if let Some((kind, bytes, _)) = candidate.allocation {
            let Some(LeaseValue {
                repr: Repr::Object(object, _),
            }) = result
            else {
                unreachable!("reserved allocation result")
            };
            let ordinal = if kind.mutable() {
                let ordinal = lease.local.ordinal;
                lease.local.ordinal += 1;
                Some(ordinal)
            } else {
                None
            };
            let slot = ObjectSlot {
                generation: object.generation,
                ordinal,
                kind,
                body,
                existing: None,
                bytes,
                incoming: 1,
                live: true,
                content: 0,
                structural: 0,
                draft_epoch: 0,
                draft: 0,
                retiring: false,
                measured: 0,
                depth: facts.depth,
            };
            if lease.local.free.pop().is_some() {
                lease.local.slots[object.slot as usize] = slot;
            } else {
                lease.local.slots.push(slot);
            }
        }
        if let Some(plan) = candidate.member {
            let LeaseOp::Member { object, .. } = candidate.op else {
                unreachable!("member phase")
            };
            let index = object.slot as usize;
            let slot = &mut lease.local.slots[index];
            slot.bytes = plan.body_bytes;
            if plan.old.is_some() || plan.next.is_some() {
                slot.content += 1;
            }
            slot.structural += u64::from(plan.structural);
            if let Some(existing) = &mut slot.existing {
                if plan.structural {
                    if plan.next.is_some() {
                        existing.len += 1;
                        existing.next += 1;
                    } else {
                        existing.len -= 1;
                    }
                }
                if !list.is_empty() {
                    for (position, old, next) in list {
                        let address = Address::List(position);
                        if let Some(overlay) = existing.overlays.get_mut(&address) {
                            overlay.current = next;
                        } else {
                            existing.overlays.insert(
                                address,
                                OverlayMember {
                                    original: old,
                                    current: next,
                                },
                            );
                        }
                    }
                } else if let Some(overlay) = existing.overlays.get_mut(&plan.address) {
                    overlay.current = plan.next;
                    if overlay.original.is_none() && overlay.current.is_none() {
                        existing.overlays.remove(&plan.address);
                    }
                } else if plan.old.is_some() || plan.next.is_some() {
                    existing.overlays.insert(
                        plan.address,
                        OverlayMember {
                            original: plan.old,
                            current: plan.next,
                        },
                    );
                }
            } else {
                match (&mut slot.body, plan.address) {
                    (Some(LocalBody::Values(values)), Address::List(at)) => {
                        if let Some(next) = plan.next {
                            if at as usize == values.len() {
                                values.push(next.value);
                            } else {
                                values[at as usize] = next.value;
                            }
                        } else {
                            values.remove(at as usize);
                        }
                    }
                    (Some(LocalBody::Cell(value)), Address::Variable) => {
                        let Some(next) = plan.next else {
                            unreachable!("reserved variable member")
                        };
                        *value = next.value
                    }
                    (Some(LocalBody::Table(table)), Address::Table(key)) => {
                        match (plan.old, plan.next) {
                            (Some(_), Some(next)) => {
                                let at = table.index[&key];
                                table.entries[at] = next;
                            }
                            (None, Some(next)) => {
                                table.next = next.sequence + 1;
                                table.index.insert(key, table.entries.len());
                                table.entries.push(next);
                            }
                            (Some(_), None) => {
                                let Some(at) = table.index.remove(&key) else {
                                    unreachable!("reserved table removal")
                                };
                                table.entries.remove(at);
                                for place in table.index.values_mut() {
                                    if *place > at {
                                        *place -= 1;
                                    }
                                }
                            }
                            (None, None) => {}
                        }
                    }
                    _ => unreachable!("reserved member kind"),
                }
            }
        }
        for index in &lease.local.touched {
            let slot = &mut lease.local.slots[*index as usize];
            slot.incoming = slot.draft as u64;
            if slot.retiring {
                slot.live = false;
                if slot.existing.is_none() {
                    if let Some(body) = &mut slot.body {
                        body.clear();
                    }
                    lease.local.free.push(*index);
                }
            }
        }
        lease.local.peak = lease
            .local
            .peak
            .max(lease.local.delta + i128::from(facts.peak));
        lease.local.delta += facts.delta;
        lease.local.version += 1;
        result
    }
}
