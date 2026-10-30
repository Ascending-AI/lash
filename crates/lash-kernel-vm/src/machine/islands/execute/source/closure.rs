//! A closure owns its actual captured cells before its prepared body is installed.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{
    JournalEvent, Program, PublicationValue as PV, VirtualBody, VirtualEntity,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

fn capture_buffer<T>(len: usize) -> Result<Vec<T>, HeapFault> {
    let mut values = Vec::new();
    values.try_reserve(len).map_err(|_| HeapFault::Exhausted {
        resource: doc::heap::HostResource::StagingBytes,
    })?;
    Ok(values)
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_closure_construction(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &sites::Closure,
    ) -> Result<ValuePublication, HeapFault> {
        if self.tape.is_some() || self.projection().is_some() || self.arena.any_cold() {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(statement) = frame.state.statement else {
            return Ok(ValuePublication::Unavailable);
        };
        let (stream, _, _) = self.exe.statement(statement);
        let Some(at) = stream
            .source_sites
            .get(site.pc as usize)
            .and_then(Option::as_ref)
        else {
            return Ok(ValuePublication::Unavailable);
        };
        if !matches!(stream.ops.get(site.pc as usize), Some(lash_kernel_exec::Op::Closure { dst, code }) if dst.0 == site.reg && code.0 == site.code.0)
        {
            return Ok(ValuePublication::Unavailable);
        }
        let target = self.exe.code(site.code);
        if target.created.as_ref() != Some(at)
            || !self.storage_room(self.arena.sequence_bound(target.captures.len()))
            || !self.source_operand_transfer(frame, at)
        {
            return Ok(ValuePublication::Unavailable);
        }
        let lash_kernel_value::sites::Unit::Library(function) = target.site.unit else {
            return Ok(ValuePublication::Unavailable);
        };
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let Ok(unpaid) = source.next_after_commit(at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let mut captures = capture_buffer(target.captures.len())?;
        for capture in &target.captures {
            let cell = match frame.slots.get(capture.outer_at as usize) {
                Some(SlotState::Cell(cell)) => *cell,
                _ if frame.state.callee.captured != RESIDENT => {
                    let Some(index) = frame
                        .state
                        .callee
                        .plan
                        .captures
                        .iter()
                        .position(|at| *at == capture.outer_at)
                    else {
                        return Ok(ValuePublication::Unavailable);
                    };
                    self.scratch.captured[frame.state.callee.captured as usize + index].1
                }
                _ => return Ok(ValuePublication::Unavailable),
            };
            captures.push(cell);
        }
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(ValuePublication::Unavailable);
        };
        self.source_bindings(frame)?;
        self.logical.flush(self.arena, self.heap)?;
        let mut entities = capture_buffer(captures.len())?;
        for handle in &captures {
            let Some(entity) = self
                .arena
                .facts(*handle)
                .ok()
                .and_then(|facts| facts.entity)
            else {
                return Ok(ValuePublication::Unavailable);
            };
            entities.push(entity);
        }
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("closure construction has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("closure source owners failed verification"))?;
        let output = slots
            .root(
                &room::Owner::Operand(at.site.clone()),
                self.task,
                activation.invocation.id(),
            )
            .ok_or_else(|| malformed("closure result has no canonical source owner"))?;
        let entity = doc::heap::EntityId {
            family: doc::heap::FamilyId::PRIMARY,
            ordinal: self.arena.next_ordinal(),
        };
        let event = JournalEvent {
            outputs: vec![(output, PV::Page(doc::heap::PageValue::Closure(entity)))],
            entities: vec![VirtualEntity {
                entity,
                body: VirtualBody::Closure(entities.clone()),
            }],
            ..Default::default()
        };
        let store = self.logical.ledger();
        if store.borrow().holding(output, self.heap.pages())?.is_some() {
            return Ok(ValuePublication::Declined);
        }
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: captures.clone(),
                    values: [(
                        Measure::CaptureEdges(at.site.clone()),
                        captures.len() as i128,
                    )]
                    .into(),
                    ..Default::default()
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.closure-construction");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        entities.encode(&mut encoding);
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
        let link = frame.state.callee.link;
        let identity = std::sync::Arc::new(doc::arena::ClosureIdentity {
            capture_names: target.capture_names.clone(),
            callable: lash_kernel_value::names::CallableId { link, function },
            site: doc::linked_site::LinkedSite {
                link,
                site: target
                    .created
                    .clone()
                    .ok_or_else(|| malformed("verified closure site disappeared"))?,
            },
        });
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
            publication::ClosureConstruction {
                entity,
                identity,
                captures,
                entities,
                code: doc::arena::LocalCode {
                    executable: self.exe.local(),
                    code: site.code.0,
                },
            },
            |arena, _, edits, storage| {
                debug_assert!(edits.is_empty());
                storage.batch.publish(arena);
                *result = Value::Closure(storage.handle);
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::closure_constructed();
        }
        Ok(if published {
            ValuePublication::Published(*result)
        } else {
            ValuePublication::Declined
        })
    }
}
