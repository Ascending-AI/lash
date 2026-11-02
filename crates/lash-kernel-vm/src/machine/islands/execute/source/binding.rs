//! A local binding transfers the evaluated RHS occurrence at its source Commit.
use super::*;
use crate::admission::Allowance;
use crate::machine::islands::compiled::WriteSite;
use lash_kernel_state::kernel_internals::logical::{
    JournalEvent, Program, PublicationValue as PV, VirtualBody, VirtualEntity,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_binding(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &WriteSite,
        value: Value,
    ) -> Result<Option<bool>, HeapFault> {
        if site.accumulates
            || self.tape.is_some()
            || self.projection().is_some()
            || (!site.bind && matches!(frame.slots[site.at], SlotState::Empty))
        {
            return Ok(None);
        }
        if site.shared && !site.bind {
            return self.publish_cell_store(frame, site, value);
        }
        let Some(statement) = frame.state.statement else {
            return Ok(None);
        };
        let (stream, statement, _) = self.exe.statement(statement);
        let Some(at) = stream
            .source_sites
            .get(site.pc as usize)
            .and_then(Option::as_ref)
        else {
            return Ok(None);
        };
        let code = self.exe.code(frame.state.callee.plan.code);
        let source = &code.source_room;
        if site.shared && (source.persistent.contains_key(at) || !self.cell_room(1)) {
            return Ok(None);
        }
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(None);
        };
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(None);
        };
        let Some((_, rhs)) = roles.iter().find(|(reg, _)| reg.0 == site.reg) else {
            return Ok(None);
        };
        let Some(unpaid) = self.source_next_statement(frame) else {
            return Ok(None);
        };
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("binding has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("binding source owners failed verification"))?;
        let selector = room::Owner::Operand((**rhs).clone());
        let input = slots
            .root(&selector, self.task, activation.invocation.id())
            .ok_or_else(|| malformed("binding RHS has no source owner"))?;
        let output = source
            .binding_root(
                code.slots_by_at[site.at],
                self.task,
                activation.invocation.id(),
            )
            .ok_or_else(|| malformed("binding has no canonical slot"))?;
        let invocation = activation.invocation.id();
        if !site.shared
            && value.handle().is_none()
            && match frame.slots[site.at] {
                SlotState::Empty => true,
                SlotState::Value(old) => old.handle().is_none(),
                SlotState::Cell(_) => false,
            }
            && self
                .logical
                .recorded(input)
                .is_some_and(|held| held.same(&value))
            && interval
                .binding_move(source, rhs)
                .is_some_and(|owner| slots.root(owner, self.task, invocation) == Some(output))
        {
            // The RHS operand is still in the open region, so its move joins
            // the region too: it adds no peak above the region's admitted
            // envelope, and only the close charge and the window stand.
            if site.bind && self.logical.holds(output, self.heap.pages())? {
                return Ok(Some(false));
            }
            let close = self
                .accounting
                .then_some(site.close)
                .flatten()
                .map_or(0, |close| u64::from(close.total));
            let depth = (self.base + u64::from(self.depth)).saturating_sub(1);
            let Some(end) = self.charged.checked_add(close) else {
                return Ok(Some(false));
            };
            let Some(frontier) = frame
                .state
                .source
                .as_mut()
                .and_then(|activation| activation.frontier.as_mut())
            else {
                return Ok(None);
            };
            if end > self.bound || depth > self.call_depth {
                return Ok(Some(false));
            }
            frontier.proven()?;
            self.logical.replace(input, None);
            self.logical
                .replace(output, Some(SourceValue::Value(value)));
            self.charged = end;
            frame.slots[site.at] = SlotState::Value(value);
            return Ok(Some(true));
        }
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("binding has no source activation"))?;
        self.logical.flush(self.arena, self.heap)?;
        let entity = site.shared.then(|| doc::heap::EntityId {
            family: doc::heap::FamilyId::PROVISIONAL,
            ordinal: self.arena.next_provisional().ordinal,
        });
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(input, self.heap.pages())?.is_none() {
            return Ok(None);
        }
        if site.bind && ledger.holding(output, self.heap.pages())?.is_some() {
            return Ok(Some(false));
        }
        if entity.is_none()
            && value
                .handle()
                .is_none_or(|handle| self.arena.content_version(handle).is_some())
            && interval.binding_move(source, rhs).is_some_and(|owner| {
                slots.root(owner, self.task, activation.invocation.id()) == Some(output)
            })
            && let Some(frontier) = activation.frontier.as_mut()
        {
            // The proven prefix's shared admission: a move adds no peak, so
            // only the current envelope, the close charge and the window.
            // Its journal moves the RHS holding into the binding in place,
            // releasing the binding's previous value.
            drop(ledger);
            let mut ledger = store.borrow_mut();
            let close = self
                .accounting
                .then_some(site.close)
                .flatten()
                .map_or(0, |close| u64::from(close.total));
            let depth = (self.base + u64::from(self.depth)).saturating_sub(1);
            let Some(end) = self.charged.checked_add(close) else {
                return Ok(Some(false));
            };
            if ledger.balance()?.total()? > self.memory_bound
                || end > self.bound
                || depth > self.call_depth
                || !ledger.transfer(input, output, self.heap.pages())?
            {
                return Ok(Some(false));
            }
            frontier.proven()?;
            let total = ledger.balance()?.total()?;
            drop(ledger);
            self.logical_memory.set(total);
            self.charged = end;
            frame.slots[site.at] = SlotState::Value(value);
            return Ok(Some(true));
        }
        let mut inputs = vec![input];
        let mut measures = std::collections::BTreeMap::new();
        if !site.bind {
            // The RHS already keeps every object retained by the replacement.
            // Removing the old binding therefore gives the exact source release.
            let released = ledger.release_cost(&[output], self.heap.pages())?;
            inputs.push(output);
            let event = interval
                .events()
                .next()
                .ok_or_else(|| malformed("binding event disappeared"))?;
            for measure in event.delta.measures() {
                if matches!(measure, Measure::Release(_)) {
                    measures.insert(measure.clone(), i128::from(released));
                }
            }
        }
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: value.handle().into_iter().collect(),
                    values: measures,
                    ..Default::default()
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.local-binding");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
        output.encode(&mut encoding);
        let start = SourcePosition {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: frame
                    .state
                    .control
                    .iter()
                    .filter_map(|control| match control {
                        Position::Loop { id, started, .. } => {
                            Some((self.exe.stmt(*id).site.clone(), started.saturating_sub(1)))
                        }
                        _ => None,
                    })
                    .collect(),
                action: None,
            },
        };
        let next = SourcePosition {
            at,
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: self.accounting.then_some(site.close).flatten(),
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        let make = || {
            Program::new(vec![JournalEvent {
                inputs: inputs.clone(),
                outputs: vec![(
                    output,
                    entity.map_or(PV::Existing(input), |entity| {
                        PV::Page(doc::heap::PageValue::Ref(doc::heap::Identity::Object(
                            entity,
                        )))
                    }),
                )],
                entities: entity
                    .into_iter()
                    .map(|entity| VirtualEntity {
                        entity,
                        body: VirtualBody::NativeVariable(value),
                    })
                    .collect(),
                ..Default::default()
            }])
        };
        drop(ledger);
        let prefix = publication::Prefix {
            interval,
            facts,
            start,
            next,
            allowance,
        };
        let published = if let Some(entity) = entity {
            self.publish_source_prepared(
                frame.state.callee,
                activation,
                prefix,
                make,
                publication::CellConstruction { entity, value },
                |arena, _, edits, storage| {
                    debug_assert!(edits.is_empty());
                    storage.batch.publish(arena);
                    frame.slots[site.at] = SlotState::Cell(storage.handle);
                },
            )?
        } else {
            self.publish_source(frame.state.callee, activation, prefix, make, |_, edits| {
                debug_assert!(edits.is_empty());
                frame.slots[site.at] = SlotState::Value(value);
            })?
        };
        #[cfg(any(test, feature = "testing"))]
        if published && site.shared {
            crate::machine::islands::witness::cell_constructed();
        }
        Ok(Some(published))
    }
}
