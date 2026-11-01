//! Pure packet inputs publish at their source read, before the consumer.
use super::*;
use crate::admission::Allowance;
use crate::compile::expr::SourceRead;
use crate::machine::source_publication::{PreparedStorage, StoragePreparation, native_fault};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{
    Holding, JournalEvent, Program, PublicationValue as PV,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    Measure, Point,
    publication::{Occurrence, Position as SourcePosition},
};

use crate::machine::source_publication::literal_storage::{Literal, PreparedLiteral};

#[derive(Clone, Copy)]
enum Input<'a> {
    Native(Value),
    Literal(&'a doc::Literal),
}
impl Input<'_> {
    fn native(self) -> Option<Value> {
        match self {
            Self::Native(value) => Some(value),
            Self::Literal(_) => None,
        }
    }
}
struct ReadStorage<'a> {
    value: Input<'a>,
    link: lash_kernel_value::content_ids::LinkId,
    physical_room: u64,
    owner: Owner,
    at: Point,
    heap: &'a lash_kernel_state::kernel_internals::PagedHeap,
}
enum PreparedRead {
    Native { value: Value, bytes: u64, at: Point },
    Literal(PreparedLiteral),
}
impl PreparedRead {
    fn publish(self, arena: &mut Arena) -> Value {
        match self {
            Self::Native { value, .. } => value,
            Self::Literal(prepared) => {
                prepared.batch.publish(arena);
                prepared.value
            }
        }
    }
}
impl StoragePreparation for ReadStorage<'_> {
    type Prepared = PreparedRead;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, PreparedRead>>, HeapFault> {
        let value = match self.value {
            Input::Native(value) => value,
            Input::Literal(literal) => {
                return Ok(Literal {
                    literal,
                    link: self.link,
                    output: self.owner,
                    at: self.at,
                    physical_room: self.physical_room,
                    heap: self.heap,
                }
                .prepare(arena, actual)?
                .map(|prepared| PreparedStorage {
                    registration: prepared.registration,
                    storage: PreparedRead::Literal(prepared.storage),
                    actual: prepared.actual,
                }));
            }
        };
        let mut registration = arena.registration().map_err(native_fault)?;
        let holding = Holding::from_native(registration.arena(), self.heap.pages(), &value)?;
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.inputs.is_empty()
            || event.outputs.len() != 1
            || event.outputs[0].0 != self.owner
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
            || !event.scratch.is_empty()
        {
            return Ok(None);
        }
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedRead::Native {
                value,
                bytes: holding.bytes(),
                at: self.at,
            },
            actual,
        }))
    }
    fn facts(storage: &PreparedRead, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        match storage {
            PreparedRead::Native { bytes, at, .. } => {
                let facts = facts
                    .0
                    .get_mut(at)
                    .ok_or_else(|| malformed("packet read facts disappeared"))?;
                facts.values.insert(
                    Measure::Local(room::Owner::Operand(at.site.clone())),
                    i128::from(*bytes),
                );
                Ok(())
            }
            PreparedRead::Literal(prepared) => {
                <Literal<'_, '_> as StoragePreparation>::facts(prepared, facts)
            }
        }
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands) fn source_read(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        value: Value,
    ) -> Result<bool, Stop> {
        self.source_input(frame, read, reg, Input::Native(value))
    }

    pub(in crate::machine::islands) fn source_literal(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        literal: &doc::Literal,
    ) -> Result<bool, Stop> {
        self.source_input(frame, read, reg, Input::Literal(literal))
    }

    fn source_input(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        value: Input<'_>,
    ) -> Result<bool, Stop> {
        let Some(statement_id) = frame.state.statement else {
            return Ok(false);
        };
        if frame
            .state
            .control
            .iter()
            .any(|control| matches!(control, Position::Try { .. }))
            || frame
                .state
                .source
                .as_ref()
                .is_some_and(|source| source.cleaning.is_some())
        {
            return Ok(false);
        }
        let (stream, statement, _) = self.exe.statement(statement_id);
        let range = match statement.form {
            crate::compile::Form::While { test, .. }
                if read.pc() >= test.start && read.pc() < test.end() =>
            {
                test
            }
            _ => statement.ops,
        };
        if read.pc() < range.start || read.pc() >= range.end() || !read.verify(stream, reg) {
            return Err(self.halt(malformed("packet read has another source prefix").into()));
        }
        // The original unpaid quote is a charge guard, never a memory price.
        // On a miss source replays that exact prefix before acquiring this root.
        if !self.fits(read.quote()) {
            return Err(self.at(read.unpaid()));
        }
        let Some(Some(site)) = stream.source_sites.get(read.pc() as usize) else {
            return Ok(false);
        };
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let Ok((at, unpaid, interval)) = source.compile_leaf(site) else {
            return Ok(false);
        };
        let cell = match stream.ops[read.pc() as usize] {
            Op::Local { local, .. } => match frame.slots[local.at as usize] {
                SlotState::Cell(cell) => Some(cell),
                _ => None,
            },
            _ => None,
        };
        self.logical
            .flush(self.arena, self.heap)
            .map_err(|error| self.halt(error.into()))?;
        let activation =
            frame.state.source.as_mut().ok_or_else(|| {
                self.halt(malformed("packet read has no source activation").into())
            })?;
        let slots = source
            .owner_slots()
            .map_err(|_| self.halt(malformed("packet read owners failed verification").into()))?;
        let output = slots
            .root(
                &room::Owner::Operand(at.site.clone()),
                self.task,
                activation.invocation.id(),
            )
            .ok_or_else(|| self.halt(malformed("packet read has no canonical owner").into()))?;
        if self
            .logical
            .ledger()
            .borrow()
            .holding(output, self.heap.pages())
            .map_err(|error| self.halt(error.into()))?
            .is_some()
        {
            return Err(self.at(read.unpaid()));
        }
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.packet-read");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        encoding.unsigned(u64::from(read.unpaid()));
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
                    .enumerate()
                    .filter_map(|(index, control)| match control {
                        Position::Loop {
                            id,
                            started,
                            cursor,
                            ..
                        } => Some((
                            self.exe.stmt(*id).site.clone(),
                            started.saturating_sub(u64::from(
                                cursor.is_some() || index + 1 < frame.state.control.len(),
                            )),
                        )),
                        _ => None,
                    })
                    .collect(),
                action: None,
                departure: None,
            },
        };
        let next = SourcePosition {
            at: at.clone(),
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
        #[cfg(any(test, feature = "testing"))]
        let callee_read = at.phase == room::Phase::Open;
        let physical_room = self
            .ceiling
            .saturating_sub(self.register_bytes)
            .saturating_sub(self.arena.held())
            .saturating_sub(self.transient);
        let result = &mut frame.regs[reg as usize];
        let published = self
            .publish_source_prepared(
                frame.state.callee,
                activation,
                publication::Prefix {
                    interval,
                    facts: LeafExecution(
                        [(
                            at.clone(),
                            LeafFacts {
                                dependencies: value
                                    .native()
                                    .and_then(|value| value.handle())
                                    .into_iter()
                                    .chain(cell)
                                    .collect(),
                                ..Default::default()
                            },
                        )]
                        .into(),
                    ),
                    start,
                    next,
                    allowance,
                },
                || {
                    Program::new(vec![JournalEvent {
                        outputs: value
                            .native()
                            .map(|value| (output, PV::Native(value)))
                            .into_iter()
                            .collect(),
                        ..Default::default()
                    }])
                },
                ReadStorage {
                    value,
                    link: frame.state.callee.link,
                    physical_room,
                    owner: output,
                    at,
                    heap: self.heap,
                },
                |arena, _, edits, storage| {
                    debug_assert!(edits.is_empty());
                    *result = storage.publish(arena);
                },
            )
            .map_err(|error| self.halt(error.into()))?;
        if !published {
            return Err(self.at(read.unpaid()));
        }
        #[cfg(any(test, feature = "testing"))]
        {
            crate::machine::islands::witness::packet_read_published();
            if callee_read {
                crate::machine::islands::witness::callee_read_published();
            }
        }
        Ok(true)
    }

    pub(in crate::machine::islands) fn source_inputs<const N: usize>(
        &mut self,
        frame: &mut Values<'_, '_>,
        reads: &[Read],
        values: &mut [Value; N],
    ) -> Result<(), Stop> {
        self.inputs(frame.slots, reads, values)?;
        for (index, read) in reads.iter().enumerate() {
            if let Err(stop) = self.source_read(frame, read.source(), read.reg(), values[index]) {
                for (read, value) in reads[..index].iter().zip(&values[..index]) {
                    frame.regs[read.reg() as usize] = *value;
                }
                return Err(stop);
            }
        }
        Ok(())
    }
}
