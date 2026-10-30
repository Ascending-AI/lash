//! A source list owns its canonical graph before detached storage is installed.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{
    Holding, JournalEvent, Program, PublicationValue as PV, VirtualBody, VirtualEntity,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_list_construction(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &sites::Make,
    ) -> Result<ValuePublication, HeapFault> {
        if site.make != crate::compile::expr::Make::List
            || self.arena.any_cold()
            || !self.resident_room(self.arena.sequence_bound(site.members))
        {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(statement) = frame.state.statement else {
            return Ok(ValuePublication::Unavailable);
        };
        let (stream, statement, _) = self.exe.statement(statement);
        let Some(at) = stream
            .source_sites
            .get(site.pc as usize)
            .and_then(Option::as_ref)
        else {
            return Ok(ValuePublication::Unavailable);
        };
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        if !source.local_binding_transfer(at) {
            return Ok(ValuePublication::Unavailable);
        }
        let Ok(unpaid) = source.next_after_commit(at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(ValuePublication::Unavailable);
        };
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("construction has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("construction source owners failed verification"))?;
        let root = |selector: &room::Owner| {
            slots
                .root(selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("construction operand has no canonical owner"))
        };
        let mut inputs = Vec::new();
        let mut values = Vec::new();
        for reg in &site.items {
            let Some((_, operand)) = roles.iter().find(|(input, _)| input.0 == *reg) else {
                return Ok(ValuePublication::Unavailable);
            };
            inputs.push(root(&room::Owner::Operand((**operand).clone()))?);
            values.push(frame.regs[*reg as usize]);
        }
        let output = root(&room::Owner::Operand(at.site.clone()))?;
        for (reg, operand) in roles.iter() {
            self.logical.replace(
                root(&room::Owner::Operand((**operand).clone()))?,
                Some(SourceValue::Value(frame.regs[reg.ix()])),
            );
        }
        self.logical.flush(self.arena, self.heap)?;
        // Canonical occurrences register all child identities before selecting
        // the next fresh identity. No body or price is inferred from capacity.
        let members = values.iter().try_fold(0u64, |sum, value| {
            sum.checked_add(Holding::from_native(self.arena, self.heap.pages(), value)?.bytes())
                .ok_or_else(|| malformed("construction members overflow"))
        })?;
        let entity = doc::heap::EntityId {
            family: doc::heap::FamilyId::PRIMARY,
            ordinal: self.arena.next_ordinal(),
        };
        let event = JournalEvent {
            outputs: vec![(output, PV::Page(doc::heap::PageValue::List(entity)))],
            entities: vec![VirtualEntity {
                entity,
                body: VirtualBody::NativeList(values.clone()),
            }],
            ..Default::default()
        };
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(output, self.heap.pages())?.is_some() {
            return Ok(ValuePublication::Declined);
        }
        // Keep inputs while introducing the new body in an unpublished graph.
        // Its release excludes children retained by the new list, including aliases.
        let preview = Program::new(vec![event.clone()]);
        let Some(preview) = preview.evaluate(&ledger, self.arena) else {
            return Ok(ValuePublication::Declined);
        };
        let preview = preview
            .prepare(
                &ledger,
                self.arena
                    .registration()
                    .map_err(publication::native_fault)?,
                self.heap,
                Program::new(vec![event.clone()]),
            )
            .map_err(super::unit::member_fault)?;
        let released = preview.release_cost(&inputs, self.heap.pages())?;
        drop(preview);
        let event = JournalEvent { inputs, ..event };
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: values.iter().filter_map(Value::handle).collect(),
                    values: [
                        (Measure::Members(at.site.clone()), i128::from(members)),
                        (
                            Measure::Release(
                                interval
                                    .events()
                                    .next()
                                    .ok_or_else(|| malformed("construction event disappeared"))?
                                    .inputs
                                    .clone(),
                            ),
                            i128::from(released),
                        ),
                    ]
                    .into(),
                    ..Default::default()
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.list-construction");
        at.site.encode(&mut encoding);
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
                departure: None,
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
            close: None,
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        drop(ledger);
        let result = &mut frame.regs[site.reg as usize];
        let published = self.publish_source_prepared(
            frame.state.callee,
            activation,
            publication::Prefix {
                interval,
                facts,
                start,
                next,
                allowance,
            },
            || Program::new(vec![event.clone()]),
            publication::ListConstruction { entity, values },
            |arena, edits, storage| {
                debug_assert!(edits.is_empty());
                storage.batch.publish(arena);
                *result = storage.value;
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::construction_published();
        }
        Ok(if published {
            ValuePublication::Published(frame.regs[site.reg as usize])
        } else {
            ValuePublication::Declined
        })
    }
}
