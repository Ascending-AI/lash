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

use crate::machine::source_publication::construction::storage::{Construction, ConstructionValue};

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
    value: Value,
    owner: Owner,
    at: Point,
    placement: &'a room::placement::BindingPlacement,
    invocation: &'a Invocation,
    slots: &'a [SlotState],
    code: crate::compile::CodeId,
    original: Option<(
        &'a std::sync::Arc<room::charges::OriginalPrefix>,
        Vec<Value>,
    )>,
}
impl ReadStorage<'_> {
    fn bind(
        &self,
        arena: &mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<room::publication::BoundEvent>, HeapFault> {
        let value = self.value;
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
        Ok(Some(bound))
    }
}
impl BoundaryPreparation for ReadStorage<'_> {
    type Prepared = Value;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, Value)>, HeapFault> {
        let (frontier, ledger, heap) = (entry.frontier, entry.ledger, entry.heap);
        let Some(bound) = self.bind(arena, entry)? else {
            return Ok(None);
        };
        let registration = arena.registration().map_err(native_fault)?;
        match bound.prepare(frontier, ledger, registration, heap) {
            Ok(prepared) => Ok(Some((prepared, self.value))),
            Err(room::publication::PublicationError::Miss(_)) => Ok(None),
            Err(room::publication::PublicationError::Heap(error)) => Err(error),
        }
    }
    fn prepare_remaining<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
        prior: &room::charges::AcceptedOriginalPrefix,
    ) -> Result<Option<(room::publication::OriginalPrepared<'a>, Value)>, HeapFault> {
        let Some((program, registers)) = self.original.as_ref() else {
            return Ok(None);
        };
        let (frontier, ledger, heap) = (entry.frontier, entry.ledger, entry.heap);
        let Some(bound) = self.bind(arena, entry)? else {
            return Ok(None);
        };
        let bound = match bound.with_original_prefix(
            frontier,
            ledger,
            arena,
            heap,
            program,
            registers,
            Some(prior),
        ) {
            Ok(bound) => bound,
            Err(room::publication::PublicationError::Miss(_)) => return Ok(None),
            Err(room::publication::PublicationError::Heap(error)) => return Err(error),
        };
        let registration = arena.registration().map_err(native_fault)?;
        match bound.prepare(frontier, ledger, registration, heap) {
            Ok(prepared) => Ok(Some((prepared, self.value))),
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
    ) -> Result<(), Stop> {
        if self.source_input(frame, read, reg, Input::Native(value), None)? {
            Ok(())
        } else {
            Err(self.at(read.unpaid()))
        }
    }

    pub(in crate::machine::islands::execute) fn source_constant(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        value: Value,
        original: Option<&std::sync::Arc<room::charges::OriginalPrefix>>,
    ) -> Result<bool, Stop> {
        let Some(original) = original else {
            // Without a verified original prefix the source evaluator must
            // enter before this read acquires an owner or pays a close.
            return Ok(false);
        };
        let literal = match value {
            Value::Null => doc::Literal::Null,
            Value::Absent => doc::Literal::Absent,
            Value::Bool(value) => doc::Literal::Bool(value),
            Value::Int(value) => doc::Literal::Int(value.into()),
            Value::Float(value) => doc::Literal::Float(value),
            Value::Text(value) => doc::Literal::Text(value.as_str().into()),
            _ => return Err(self.halt(malformed("resident literal has another value kind").into())),
        };
        self.source_input(frame, read, reg, Input::Literal(&literal), Some(original))
    }

    pub(in crate::machine::islands::execute) fn source_literal(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        literal: &doc::Literal,
        original: &std::sync::Arc<room::charges::OriginalPrefix>,
    ) -> Result<bool, Stop> {
        self.source_input(frame, read, reg, Input::Literal(literal), Some(original))
    }

    fn source_input(
        &mut self,
        frame: &mut Values<'_, '_>,
        read: SourceRead,
        reg: u32,
        value: Input<'_>,
        original: Option<&std::sync::Arc<room::charges::OriginalPrefix>>,
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
        let quote = if original.is_some() { 0 } else { read.quote() };
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
        let transfer = self
            .logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[]);
        transfer.map_err(|error| self.halt(error.into()))?;
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
        let prefix = publication::Prefix {
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
        };
        let published = match value {
            Input::Literal(literal) => {
                let original = original.ok_or_else(|| {
                    self.halt(malformed("literal lost its original source prefix").into())
                })?;
                let value = self
                    .publish_original_construction(
                        frame.state.callee,
                        activation,
                        prefix,
                        super::construction::OriginalConstruction {
                            original,
                            registers: original.values(|reg| frame.regs[reg as usize]),
                            construction: Construction {
                                charges: None,
                                value: ConstructionValue::Literal {
                                    literal,
                                    link: frame.state.callee.link,
                                },
                                output,
                                at: at.clone(),
                                release: Box::default(),
                                inputs: vec![],
                                operands: vec![],
                                depth_limit: self.limits.value_depth(),
                                heap: self.heap,
                            },
                        },
                    )
                    .map_err(|error| self.halt(error.into()))?;
                if let Some(value) = value {
                    frame.regs[reg as usize] = value;
                }
                value.is_some()
            }
            Input::Native(value) => {
                let original = original
                    .map(|program| (program, program.values(|reg| frame.regs[reg as usize])));
                let result = &mut frame.regs[reg as usize];
                self.publish_source_boundary(
                    frame.state.callee,
                    activation,
                    prefix,
                    ReadStorage {
                        value,
                        owner: output,
                        at: at.clone(),
                        placement,
                        invocation: &invocation,
                        slots: frame.slots,
                        code,
                        original,
                    },
                    |_, _, edits, storage| {
                        debug_assert!(edits.is_empty());
                        *result = storage;
                    },
                )
                .map_err(|error| self.halt(error.into()))?
            }
        };
        if !published {
            return Err(self.at(read.unpaid()));
        }
        #[cfg(test)]
        if original.is_some() {
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
