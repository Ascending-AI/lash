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
    pub(in crate::machine::islands::execute) fn source_read(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        value: Value,
    ) -> Result<bool, Stop> {
        self.source_input(frame, read, reg, Input::Native(value))
    }

    pub(in crate::machine::islands::execute) fn source_literal(
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
        // Body and catch reads of a try without cleanup leave by their
        // ordinary raise or block suffix; a pending departure needs its recipe.
        if frame.state.control.iter().any(|control| match control {
            Position::Try { phase, .. } => {
                frame.state.callee.compiled.cleanups || matches!(phase, Phase::Finally(_))
            }
            _ => false,
        }) || frame
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
        let code = frame.state.callee.plan.code;
        let source = &self.exe.code(code).source_room;
        // The leaf's interval and operand slot are resolved once per pc.
        let leaf = stream
            .source_leaves
            .get(stream.ops.len(), read.pc() as usize, || {
                let (at, unpaid, interval) = source.compile_leaf(site).ok()?;
                let output = match source.owner_slots().ok()?.root(
                    &room::Owner::Operand(at.site.clone()),
                    self.task,
                    0,
                ) {
                    Some(Owner::Value { slot, .. }) => Some(slot),
                    _ => None,
                };
                Some(crate::compile::SourceLeaf {
                    code,
                    at,
                    unpaid,
                    interval,
                    output,
                })
            })
            .filter(|leaf| leaf.code == code);
        let owned;
        let (at, unpaid, interval, slot) = match leaf {
            Some(leaf) => (&leaf.at, &leaf.unpaid, &leaf.interval, Some(leaf.output)),
            None => {
                let Ok(leaf) = source.compile_leaf(site) else {
                    return Ok(false);
                };
                owned = leaf;
                (&owned.0, &owned.1, &owned.2, None)
            }
        };
        let cell = match stream.ops[read.pc() as usize] {
            Op::Local { local, .. } => match frame.slots[local.at as usize] {
                SlotState::Cell(cell) => Some(cell),
                _ => None,
            },
            _ => None,
        };
        let activation =
            frame.state.source.as_mut().ok_or_else(|| {
                self.halt(malformed("packet read has no source activation").into())
            })?;
        let output = match slot {
            Some(Some(slot)) => activation.invocation.owner(slot),
            Some(None) => {
                return Err(self.halt(malformed("packet read has no canonical owner").into()));
            }
            None => source
                .owner_slots()
                .map_err(|_| self.halt(malformed("packet read owners failed verification").into()))?
                .root(
                    &room::Owner::Operand(at.site.clone()),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| self.halt(malformed("packet read has no canonical owner").into()))?,
        };
        if let (Input::Native(value), Some(frontier)) = (value, activation.frontier.as_mut())
            && interval.acquisition(source).is_some()
        {
            if self
                .logical
                .holds(output, self.heap.pages())
                .map_err(|error| self.halt(error.into()))?
            {
                return Err(self.at(read.unpaid()));
            }
            // A proven pure acquisition joins the open region: the region's
            // envelope admits its bytes, the charge window and depth stand,
            // and its journal record publishes with the region.
            let holding = Holding::from_native(self.arena, self.heap.pages(), &value)
                .map_err(|error| self.halt(error.into()))?;
            if value.handle().is_some()
                && !self
                    .logical
                    .ledger()
                    .borrow()
                    .references_committed(&holding, self.heap.pages())
                    .map_err(|error| self.halt(error.into()))?
            {
                return Err(self.at(read.unpaid()));
            }
            let bytes = holding.bytes();
            let depth = (self.base + u64::from(self.depth)).saturating_sub(1);
            if self.charged > self.bound
                || depth > self.call_depth
                || !self
                    .logical
                    .admit(self.arena, self.heap, bytes, self.memory_bound)
                    .map_err(|error| self.halt(error.into()))?
            {
                return Err(self.at(read.unpaid()));
            }
            self.logical
                .replace(output, Some(SourceValue::Value(value)));
            frontier.proven().map_err(|error| self.halt(error.into()))?;
            frame.regs[reg as usize] = value;
            #[cfg(any(test, feature = "testing"))]
            {
                crate::machine::islands::witness::packet_read_published();
                if at.phase == room::Phase::Open {
                    crate::machine::islands::witness::callee_read_published();
                }
                crate::machine::islands::witness::source_loop_published(
                    &self.source_loops(&frame.state.control),
                );
            }
            return Ok(true);
        }
        self.logical
            .flush(self.arena, self.heap)
            .map_err(|error| self.halt(error.into()))?;
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
                loops: self.source_loops(&frame.state.control),
                action: None,
            },
        };
        let next = SourcePosition {
            at: at.clone(),
            unpaid: Some(unpaid.clone()),
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
                    interval: std::sync::Arc::clone(interval),
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
                    at: at.clone(),
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

    /// The read's runtime loop occurrence: an entered body counts its
    /// started iteration, a header read the one before it.
    pub(in crate::machine::islands::execute) fn source_loops(
        &self,
        controls: &[Position],
    ) -> Vec<(lash_kernel_value::sites::Site, u64)> {
        controls
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
                    started
                        .saturating_sub(u64::from(cursor.is_some() || index + 1 < controls.len())),
                )),
                _ => None,
            })
            .collect()
    }

    pub(in crate::machine::islands::execute) fn source_inputs<const N: usize>(
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
