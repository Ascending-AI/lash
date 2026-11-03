//! Native text bodies stage storage before one exact source publication.
use super::*;
use crate::admission::{Allowance, SourceResources};
use crate::compile::room::{
    Phase as SourcePhase, Point,
    publication::{
        ExactEvent, ExactRoots, ExternalFacts, Occurrence, Position as SourcePosition, Successor,
    },
};
use crate::machine::islands::cx::Cx;
use crate::machine::source_publication::native_storage;
use lash_kernel_state::kernel_internals::logical::PublicationValue;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use std::sync::Arc;

#[cfg(test)]
thread_local! {
    static TEXTS: std::cell::Cell<[u64; 2]> = const { std::cell::Cell::new([0; 2]) };
    static BODY_TEXTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static ORIGINAL_TEXTS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}
#[cfg(test)]
pub(crate) fn take_text_publications() -> [u64; 2] {
    TEXTS.with(|counts| counts.replace([0; 2]))
}

#[cfg(test)]
pub(crate) fn take_original_text_publications() -> u64 {
    ORIGINAL_TEXTS.with(|counts| counts.replace(0))
}

#[cfg(test)]
pub(crate) fn take_body_text_publications() -> u64 {
    BODY_TEXTS.with(|counts| counts.replace(0))
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_text_value(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        arguments: &[Value],
        join: bool,
    ) -> Result<ValuePublication, HeapFault> {
        let function = self.exe.lib(site.lib);
        if OBSERVING
            || self.projection().is_some()
            || self.deferred_end.is_some()
            || self.arena.boundary_due()
            || arguments.len() != 2
            || function.arity != 2
            || function.limit.is_some()
            || function.shares_params
            || (!join && function.kernels(self.exe).concat.is_none())
            || (join && !matches!(site.direct(self.exe), Some(Direct::TextJoin)))
            || !function
                .header
                .signature
                .params
                .iter()
                .zip(arguments)
                .all(|(param, value)| {
                    native_storage::resident_argument_fits(self.arena, &param.ty, *value)
                })
        {
            return Ok(ValuePublication::Unavailable);
        }
        let body = function.kernels(self.exe).text_body.as_ref();
        let primitive = self.exe.lib(body.map_or(site.lib, |body| body.primitive()));
        let LibRun::Native { native, .. } = &primitive.run else {
            return Ok(ValuePublication::Unavailable);
        };
        let room::CallSource::Native(function_id) = primitive.specialization.resource else {
            return Ok(ValuePublication::Unavailable);
        };
        let contract = match native.resource_contract(doc::Args::new(
            arguments,
            self.arena.objects(),
            &self.arena.semantics(),
        )) {
            Ok(Some(contract)) if native_storage::supports(&contract) => contract,
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            _ => return Ok(ValuePublication::Unavailable),
        };
        if contract
            .events
            .iter()
            .any(|event| matches!(event, doc::NativeResourceEvent::Entity { .. }))
        {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(statement_id) = frame.state.statement else {
            return Ok(ValuePublication::Unavailable);
        };
        let callee = frame.state.callee;
        let source = &self.exe.code(callee.plan.code).source_room;
        let (stream, statement, _) = self.exe.statement(statement_id);
        let Some(Some(at)) = stream.source_sites.get(site.pc as usize) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(op) = stream.ops.get(site.pc as usize) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let registers = op.reads();
        if registers.len() != arguments.len()
            || !registers
                .iter()
                .zip(arguments)
                .all(|(reg, value)| frame.regs[reg.0 as usize].same(value))
        {
            return Ok(ValuePublication::Unavailable);
        }
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(unpaid) = source.next_after_commit(&at.site) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Ok(interval) = source.compile_event(&at, room::ExitKind::Normal) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Ok(event) = ExactEvent::compile(source, Arc::clone(&interval), Successor::Normal)
        else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(charges) = site.charges.as_deref() else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(activation) = frame.state.source.as_mut() else {
            return Ok(ValuePublication::Unavailable);
        };
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("text source owners failed verification"))?;
        let root = |site| {
            slots
                .root(
                    &room::Owner::Operand(site),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("text has no source operand"))
        };
        let mut inputs = Vec::new();
        for reg in registers {
            let Some((_, site)) = roles.iter().find(|(input, _)| *input == reg) else {
                return Ok(ValuePublication::Unavailable);
            };
            inputs.push(root((**site).clone())?);
        }
        let output = root(at.site.clone())?;
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let mut encoding = TokenWriter::new();
        encoding.seq(2);
        encoding.text("lash.source.text");
        at.site.encode(&mut encoding);
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
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let original = activation.original.is_some();
        if let Some(receipt) = activation.original.as_ref() {
            let Some(frontier) = activation.frontier.as_mut() else {
                return Ok(ValuePublication::Declined);
            };
            if !receipt.enter_event(
                frontier,
                &mut store.borrow_mut(),
                start.clone(),
                self.charged,
            )? {
                return Ok(ValuePublication::Declined);
            }
        } else if let Some(frontier) = activation.frontier.as_mut()
            && frontier.departs(&start)
        {
            frontier.handoff(&mut store.borrow_mut(), start.clone())?;
        }
        if activation.frontier.is_none() {
            let document = self
                .exe
                .instances
                .get(&callee.link)
                .ok_or_else(|| malformed("text has no defining document"))?;
            let kernel = doc::KernelVersion::of(document.kernel())
                .ok_or_else(|| malformed("text source kernel is unsupported"))?;
            activation.frontier = Some(
                source
                    .frontier(
                        &store.borrow(),
                        self.heap,
                        &activation.invocation,
                        callee.link,
                        kernel,
                        start,
                    )
                    .map_err(|_| malformed("text frontier failed authentication"))?,
            );
        }
        let frontier = activation
            .frontier
            .as_mut()
            .ok_or_else(|| malformed("text frontier disappeared"))?;
        let mut ledger = store.borrow_mut();
        if inputs.iter().any(|input| {
            ledger
                .holding(*input, self.heap.pages())
                .ok()
                .flatten()
                .is_none()
        }) {
            return Ok(ValuePublication::Unavailable);
        }
        if ledger.holding(output, self.heap.pages())?.is_some() {
            return Ok(ValuePublication::Declined);
        }
        let safe_point = self.charged;
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: (self.base + u64::from(self.depth)).saturating_sub(1),
            limit: self.call_depth,
            safe_point,
        };
        let blank = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: None,
            strict: false,
            depth: 0,
            safe_point,
        };
        let facts = resources
            .facts(&blank)
            .ok_or_else(|| malformed("text resource facts disappeared"))?;
        let Some(mut storage) = Cx::prepare_text_prefix(
            self.arena,
            &ledger,
            self.heap,
            arguments,
            &[],
            join.then_some((0, 1)),
            if join { &[] } else { &[(0, 1, false)] },
            &[(output, 2)],
            &inputs,
        )?
        else {
            return Ok(ValuePublication::Unavailable);
        };
        let [evaluated] = storage.evaluations() else {
            return Err(malformed("text native has another evaluation count"));
        };
        let value = evaluated.result;
        if !crate::compile::import_result_fits(&function.header.signature.result, value) {
            return Ok(ValuePublication::Declined);
        }
        debug_assert_eq!(evaluated.install != 0, matches!(value, Value::LongText(_)));
        debug_assert!(evaluated.work == 0 || evaluated.work == evaluated.summaries[2].utf8_bytes);
        let arena = storage.arena();
        let debt = charges.bind_measured(
            &[room::charges::FormulaValues {
                arguments: &evaluated.arguments,
                result: value,
            }],
            |_, input, measure| {
                let value = match input {
                    crate::compile::Source::Arg(at) => evaluated.arguments.get(at),
                    crate::compile::Source::Result => Some(&value),
                    crate::compile::Source::Nothing => None,
                };
                value.map_or(Some(0), |value| match measure {
                    doc::Measure::Size => crate::data::view_size(arena, value).ok(),
                    doc::Measure::DeepSize => crate::data::view_deep_size(arena, value).ok(),
                    doc::Measure::NestedSize => crate::data::view_nested_size(arena, value).ok(),
                    doc::Measure::Magnitude => Some(crate::data::view_magnitude(arena, value)),
                })
            },
        );
        let Some(debt) = debt else {
            return Ok(ValuePublication::Declined);
        };
        let Some(mut allowance) = debt.native_allowance(safe_point) else {
            return Ok(ValuePublication::Declined);
        };
        allowance.depth = allowance.depth.max(match site.depth {
            Depth::None => 0,
            Depth::Operation(extra) if extra <= 1 => 1,
            Depth::Operation(extra) | Depth::Template(extra) => u64::from(extra) + 1,
        });
        let released = storage.release_cost(&ledger, self.heap)?;
        let (called, completion) = if let Some(body) = body {
            let Some(call) = body.bind(
                self.exe,
                &ledger,
                storage.arena(),
                &activation.invocation,
                arguments,
                value,
            )?
            else {
                return Ok(ValuePublication::Declined);
            };
            let (room, completion) = call.room();
            allowance.depth = allowance.depth.max(completion.depth());
            (room, Some(completion))
        } else {
            (
                room::CheckedRoom::native(function_id, &contract)
                    .ok_or_else(|| malformed("text contract failed verification"))?,
                None,
            )
        };
        let scratch = if body.is_none() {
            native_storage::scratch_events(output, &contract)?
        } else {
            vec![]
        };
        let mut execution = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: arguments
                        .iter()
                        .copied()
                        .chain([value])
                        .filter_map(|value| value.handle())
                        .collect(),
                    values: [
                        (
                            room::Measure::Release(
                                interval
                                    .events()
                                    .next()
                                    .ok_or_else(|| malformed("text event disappeared"))?
                                    .inputs
                                    .clone(),
                            ),
                            i128::from(released),
                        ),
                        (
                            room::Measure::NativeScratch {
                                at: at.site.clone(),
                                function: function_id,
                            },
                            i128::from(
                                contract
                                    .peak_scratch_bytes()
                                    .ok_or_else(|| malformed("text scratch changed"))?,
                            ),
                        ),
                    ]
                    .into(),
                    call: Some((at.site.clone(), function.specialization.resource, called)),
                },
            )]
            .into(),
        );
        let bound = match event.bind_external(
            frontier,
            &ledger,
            storage.arena(),
            self.heap,
            &mut execution,
            ExactRoots {
                inputs: inputs.clone(),
                outputs: vec![(output, PublicationValue::Native(value))],
                ..Default::default()
            },
            next,
            allowance,
            facts,
            ExternalFacts {
                operands: arguments,
                arguments,
                native: body.is_none().then_some(&contract),
                scratch: &scratch,
                ..Default::default()
            },
            &debt,
        ) {
            Ok(bound) => bound,
            Err(_) => return Ok(ValuePublication::Declined),
        };
        let memory_bound = self.memory_bound;
        let published = storage
            .boundary(&mut ledger, self.heap, |transaction, ledger, apply, _| {
                use room::publication::PublicationError;
                let transaction = match completion {
                    Some(completion) => completion.finish(ledger, self.heap, transaction)?,
                    None => transaction,
                };
                let balance = if let Some(receipt) = activation.original.as_ref() {
                    let bound = match bound.with_original_native(frontier, ledger, receipt, &debt) {
                        Ok(bound) => bound,
                        Err(PublicationError::Miss(_)) => return Ok(false),
                        Err(PublicationError::Heap(error)) => return Err(error),
                    };
                    let prepared = match bound.finish_external(frontier, ledger, transaction) {
                        Ok(prepared) => prepared,
                        Err(PublicationError::Miss(_)) => return Ok(false),
                        Err(PublicationError::Heap(error)) => return Err(error),
                    };
                    let Some(accepted) =
                        prepared.admit(frontier, ledger, resources, memory_bound)?
                    else {
                        return Ok(false);
                    };
                    accepted.publish_original_event(
                        frontier,
                        ledger,
                        &mut activation.original,
                        |arena, edits| apply(arena, edits),
                        self.recorder,
                    )
                } else {
                    let prepared = match bound.finish_external(frontier, ledger, transaction) {
                        Ok(prepared) => prepared,
                        Err(PublicationError::Miss(_)) => return Ok(false),
                        Err(PublicationError::Heap(error)) => return Err(error),
                    };
                    let Some(accepted) =
                        prepared.admit(frontier, ledger, resources, memory_bound)?
                    else {
                        return Ok(false);
                    };
                    accepted.publish_event(frontier, ledger, |arena, edits| apply(arena, edits))
                }
                .map_err(|error| match error {
                    PublicationError::Heap(error) => error,
                    PublicationError::Miss(reason) => malformed(reason),
                })?;
                self.logical_memory.set(balance.total()?);
                Ok(true)
            })?
            .is_some_and(|(published, _)| published);
        if !published {
            return Ok(ValuePublication::Declined);
        }
        frame.regs[site.reg as usize] = value;
        if !original && let Some(tape) = self.recorder {
            debt.record(tape);
        }
        #[cfg(test)]
        if original {
            ORIGINAL_TEXTS.with(|counts| counts.set(counts.get() + 1));
        }
        if body.is_some() {
            witnessed!(TextAppend, Commit);
            #[cfg(test)]
            BODY_TEXTS.with(|counts| counts.set(counts.get() + 1));
            #[cfg(test)]
            crate::machine::islands::TEXT_APPENDED.with(|n| n.set(n.get() + 1));
        }
        #[cfg(any(test, feature = "testing"))]
        crate::machine::islands::witness::native_text_published();
        #[cfg(test)]
        TEXTS.with(|counts| {
            let mut next = counts.get();
            next[usize::from(join)] += 1;
            counts.set(next);
        });
        Ok(ValuePublication::Published(value))
    }
}
