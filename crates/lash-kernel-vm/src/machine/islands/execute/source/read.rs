//! Pure packet inputs publish at their source read, before the consumer.
use super::*;
use crate::admission::Allowance;
use crate::compile::expr::SourceRead;
use crate::machine::source_publication::{BoundaryPreparation, ExternalEntry, native_fault};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{Holding, PublicationValue as PV};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    Measure, Point,
    publication::{Occurrence, Position as SourcePosition},
};

use crate::machine::source_publication::construction::storage::{
    Construction, ConstructionValue, Prepared,
};

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
    charges: Option<&'a room::charges::ChargeProgram>,
    value: Input<'a>,
    link: lash_kernel_value::content_ids::LinkId,
    owner: Owner,
    at: Point,
    heap: &'a lash_kernel_state::kernel_internals::PagedHeap,
    placement: &'a room::placement::BindingPlacement,
    invocation: &'a Invocation,
    slots: &'a [SlotState],
    code: crate::compile::CodeId,
    depth_limit: usize,
}
enum PreparedRead {
    Native(Value),
    Constructed(Prepared),
}
impl PreparedRead {
    fn publish(self, arena: &mut Arena) -> Value {
        match self {
            Self::Native(value) => value,
            Self::Constructed(prepared) => {
                prepared.batch.publish(arena);
                prepared.value
            }
        }
    }
}
impl BoundaryPreparation for ReadStorage<'_> {
    type Prepared = PreparedRead;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, PreparedRead)>, HeapFault> {
        let value = match self.value {
            Input::Native(value) => value,
            Input::Literal(literal) => {
                return Ok(Construction {
                    charges: self.charges,
                    value: ConstructionValue::Literal {
                        literal,
                        link: self.link,
                    },
                    output: self.owner,
                    at: self.at,
                    release: vec![].into_boxed_slice(),
                    inputs: vec![],
                    operands: vec![],
                    depth_limit: self.depth_limit,
                    heap: self.heap,
                }
                .prepare(arena, entry)?
                .map(|(prepared, storage)| (prepared, PreparedRead::Constructed(storage))));
            }
        };
        let ExternalEntry {
            event,
            frontier,
            ledger,
            heap,
            mut prefix,
            resources,
        } = entry;
        let holding = Holding::from_native(arena, heap.pages(), &value)?;
        let facts = prefix
            .facts
            .0
            .get_mut(&self.at)
            .ok_or_else(|| malformed("packet read facts disappeared"))?;
        facts.values.insert(
            Measure::Local(room::Owner::Operand(self.at.site.clone())),
            i128::from(holding.bytes()),
        );
        // Context has performed the source handoff before this view is captured.
        let Some(activation) =
            self.placement
                .capture(self.code, self.invocation, ledger, heap, self.slots)?
        else {
            return Ok(None);
        };
        let Ok(bound) = event.bind_paid_external(
            frontier,
            ledger,
            arena,
            heap,
            &mut prefix.facts,
            room::publication::ExactRoots {
                activation: Some(activation),
                inputs: vec![],
                outputs: vec![(self.owner, PV::Native(value))],
            },
            prefix.next,
            prefix.allowance,
            resources,
            room::publication::ExternalFacts::default(),
        ) else {
            return Ok(None);
        };
        let registration = arena.registration().map_err(native_fault)?;
        match bound.prepare(frontier, ledger, registration, heap) {
            Ok(prepared) => Ok(Some((prepared, PreparedRead::Native(value)))),
            Err(room::publication::PublicationError::Miss(_)) => Ok(None),
            Err(room::publication::PublicationError::Heap(error)) => Err(error),
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
        self.source_input(frame, read, reg, Input::Native(value), None)
    }

    pub(in crate::machine::islands::execute) fn source_literal(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        literal: &doc::Literal,
        charges: &room::charges::ChargeProgram,
    ) -> Result<bool, Stop> {
        self.source_input(frame, read, reg, Input::Literal(literal), Some(charges))
    }

    fn source_input(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        value: Input<'_>,
        charges: Option<&room::charges::ChargeProgram>,
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
        let quote = match charges {
            Some(charges) => charges.constructor_quote().ok_or_else(|| {
                self.halt(malformed("literal has no canonical pending charge coverage").into())
            })?,
            None => read.quote(),
        };
        if !self.fits(quote) {
            return Err(self.at(read.unpaid()));
        }
        let Some(Some(site)) = stream.source_sites.get(read.pc() as usize) else {
            return Ok(false);
        };
        let code = frame.state.callee.plan.code;
        let source = &self.exe.code(code).source_room;
        // The leaf's interval and operand slot are resolved once per pc.
        let Ok((at, unpaid, interval)) = source.compile_leaf(site) else {
            return Ok(false);
        };
        let Some(placement) = &frame.state.callee.compiled.placement else {
            return Ok(false);
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
        let output = source
            .owner_slots()
            .map_err(|_| self.halt(malformed("packet read owners failed verification").into()))?
            .root(
                &room::Owner::Operand(at.site.clone()),
                self.task,
                activation.invocation.id(),
            )
            .ok_or_else(|| self.halt(malformed("packet read has no canonical owner").into()))?;
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
        let invocation = activation.invocation.clone();
        let result = &mut frame.regs[reg as usize];
        let published = self
            .publish_source_boundary(
                frame.state.callee,
                activation,
                publication::Prefix {
                    interval: std::sync::Arc::clone(&interval),
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
                ReadStorage {
                    charges,
                    value,
                    link: frame.state.callee.link,
                    owner: output,
                    at: at.clone(),
                    heap: self.heap,
                    placement,
                    invocation: &invocation,
                    slots: frame.slots,
                    code,
                    depth_limit: self.limits.value_depth(),
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
        #[cfg(test)]
        if charges.is_some() {
            super::super::super::compiled::constructor_debt_published(2);
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
