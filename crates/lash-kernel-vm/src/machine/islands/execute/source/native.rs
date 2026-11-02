//! Native results publish their canonical occurrences and logical scratch.
use super::*;
use crate::admission::Allowance;
use crate::machine::source_publication::native_storage;
use lash_kernel_state::kernel_internals::logical::{
    Holding, JournalEvent, Program, RootValue, ScratchEvent, Slot,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

use crate::machine::islands::compiled;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_native_value(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        args: &[Value],
    ) -> Result<ValuePublication, HeapFault> {
        self.publish_native_answer(frame, site, args, false, None)
    }

    /// Prepared operators use the same source event and journal as native calls.
    /// Their canonical entry computes a value and price before either publishes.
    pub(in crate::machine::islands::execute) fn publish_prepared_value(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &OperatorSite,
        args: &[Value],
    ) -> Result<ValuePublication, HeapFault> {
        if !self.resident_room(0)
            || self.arena.any_cold()
            || site.root.is_some()
            || site.counted.is_some()
            || site.act.is_some()
            || !self.admits(site)
            || !matches!(self.exe.lib(site.lib).run, LibRun::Native { .. })
        {
            return Ok(ValuePublication::Unavailable);
        }
        // These are the actual operator coordinates, including its unpaid close
        // and compiled depth requirement; no new call or source Point is made.
        let native = NativeSite {
            lib: site.lib,
            close: site.close,
            pc: site.pc,
            reg: site.reg,
            depth: site.depth,
        };
        // A pair kernel answers what the prepared entry answers, at the
        // price its table certifies: a statement-level operation runs the
        // kernel nested operators run.
        let kernel = match args {
            [x, y] => site
                .pair(self.exe)
                .and_then(|pair| compiled::typed(site, &pair, *x, *y, self.charging, self.exe))
                .or_else(|| {
                    compiled::text_relation(site, *x, *y, self.exe, self.arena, self.charging)
                })
                .and_then(|(value, units)| Some((value, units?))),
            _ => None,
        };
        self.publish_native_answer(frame, &native, args, true, kernel)
    }

    fn publish_native_answer(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        args: &[Value],
        prepared: bool,
        kernel: Option<(Value, u64)>,
    ) -> Result<ValuePublication, HeapFault> {
        let function = self.exe.lib(site.lib);
        let text = !prepared && function.header.signature.result == lash_kernel_value::Type::Text;
        if !self.resident_room(0)
            || self.arena.any_cold()
            || function.limit.is_some()
            || function.shares_params
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
        if !self.source_operand_transfer(frame, stream, site.pc, at) {
            return Ok(ValuePublication::Unavailable);
        }
        let Ok(unpaid) = source.next_after_commit(at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(op) = stream.ops.get(site.pc as usize) else {
            return Ok(ValuePublication::Unavailable);
        };
        let registers = op.reads();
        if registers.len() != args.len() {
            return Ok(ValuePublication::Unavailable);
        }
        // Check the complete source consumer before evaluating a prepared
        // answer. Unsupported expressions retain their original handler.
        let answer = if prepared {
            let Some((entry, fast)) = &function.specialization.prepared else {
                return Ok(ValuePublication::Unavailable);
            };
            if !function
                .header
                .signature
                .params
                .iter()
                .zip(args)
                .all(|(param, value)| {
                    (param.optional && matches!(value, Value::Absent))
                        || crate::compile::import_result_fits(&param.ty, *value)
                })
            {
                return Ok(ValuePublication::Unavailable);
            }
            if let Some((value, units)) = kernel {
                if !crate::compile::import_result_fits(&function.header.signature.result, value) {
                    return Ok(ValuePublication::Unavailable);
                }
                Some((value, units))
            } else {
                let Some(answer) = entry.answer(args, self.charging) else {
                    return Ok(ValuePublication::Unavailable);
                };
                if !scalar(&answer.value)
                    || !crate::compile::import_result_fits(
                        &function.header.signature.result,
                        answer.value,
                    )
                {
                    return Ok(ValuePublication::Unavailable);
                }
                let Some(answer) = self.answered(function, fast, args, answer) else {
                    return Ok(ValuePublication::Unavailable);
                };
                Some(answer)
            }
        } else {
            None
        };
        let LibRun::Native { native, .. } = &function.run else {
            return Ok(ValuePublication::Unavailable);
        };
        let room::CallSource::Native(function_id) = function.specialization.resource else {
            return Ok(ValuePublication::Unavailable);
        };
        let contract = match native.resource_contract(doc::Args::new(
            args,
            self.arena.objects(),
            &self.arena.semantics(),
        )) {
            Ok(Some(contract)) if native_storage::supports(&contract) => contract,
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            _ => return Ok(ValuePublication::Unavailable),
        };
        let collection = contract
            .events
            .iter()
            .any(|event| matches!(event, doc::NativeResourceEvent::Entity { .. }));
        let Some(called) = room::CheckedRoom::native(function.specialization.function, &contract)
        else {
            return Ok(ValuePublication::Unavailable);
        };
        let answer = answer.or_else(|| match (site.direct(self.exe), args.first()) {
            (Some(Direct::Length(kind, units)), Some(value)) if value.kind() == kind => value
                .handle()
                .and_then(|handle| self.arena.len(handle).ok())
                .and_then(|len| i64::try_from(len).ok())
                .map(|len| (Value::Int(len), if self.charging { units } else { 0 })),
            (Some(Direct::Item(_)), _) => self.item_answer(site, args),
            (Some(Direct::Member(_)), _) => self.membership(site, args),
            _ => None,
        });
        let scratch = contract
            .peak_scratch_bytes()
            .ok_or_else(|| malformed("native scratch contract is invalid"))?;
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(input_sites) = registers
            .iter()
            .map(|register| {
                roles
                    .iter()
                    .find(|(reg, _)| reg == register)
                    .map(|(_, at)| (**at).clone())
            })
            .collect::<Option<Vec<_>>>()
        else {
            return Ok(ValuePublication::Unavailable);
        };
        if let Some((value, _)) = answer
            && value.handle().is_none()
            && args.iter().all(|arg| arg.handle().is_none())
            && !collection
            && scratch == 0
            && interval.native_commit(source)
            && let Some(joined) =
                self.join_native(frame, site, args, &contract, &input_sites, &at, value)?
        {
            #[cfg(any(test, feature = "testing"))]
            if matches!(joined, ValuePublication::Published(_)) {
                if prepared {
                    crate::machine::islands::witness::prepared_native_published();
                } else {
                    crate::machine::islands::witness::native_read_published();
                }
            }
            return Ok(joined);
        }
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("native read has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("native read owners failed verification"))?;
        let root = |at| {
            slots
                .root(
                    &room::Owner::Operand(at),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("native read operand has no source owner"))
        };
        let mut inputs = Vec::new();
        for at in input_sites {
            inputs.push(root(at)?);
        }
        let output = root(at.site.clone())?;
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        for input in &inputs {
            if ledger.holding(*input, self.heap.pages())?.is_none() {
                return Ok(ValuePublication::Unavailable);
            }
        }
        if ledger.holding(output, self.heap.pages())?.is_some() {
            return Ok(ValuePublication::Declined);
        }
        let released = if let Some((value, _)) = answer {
            ledger.release_cost_preserving(
                self.arena,
                self.heap,
                &inputs,
                &[(output, RootValue::Value(&value))],
            )?
        } else {
            ledger.release_cost(&inputs, self.heap.pages())?
        };
        let event = interval
            .events()
            .next()
            .ok_or_else(|| malformed("native read event disappeared"))?;
        let release = event.inputs.clone();
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: args
                        .iter()
                        .copied()
                        .chain(answer.map(|(value, _)| value))
                        .filter_map(|value| value.handle())
                        .collect(),
                    values: [
                        (Measure::Release(event.inputs.clone()), i128::from(released)),
                        (
                            Measure::NativeScratch {
                                at: at.site.clone(),
                                function: function_id,
                            },
                            i128::from(scratch),
                        ),
                    ]
                    .into(),
                    call: Some((at.site.clone(), function.specialization.resource, called)),
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.native");
        at.site.encode(&mut encoding);
        inputs.encode(&mut encoding);
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
            units: if self.accounting {
                answer.map_or(0, |(_, units)| units)
            } else {
                0
            },
            close: self.accounting.then_some(site.close).flatten(),
            strict: false,
            depth: match site.depth {
                Depth::None => 0,
                Depth::Operation(extra) if extra <= 1 => 1,
                Depth::Operation(extra) | Depth::Template(extra) => u64::from(extra) + 1,
            },
            safe_point: self.charged,
        };
        let scratch_events = if scratch == 0 {
            vec![]
        } else {
            let Owner::Value {
                task,
                invocation,
                slot: Slot::Operand(index),
            } = output
            else {
                return Err(malformed("native output has no operand index"));
            };
            let owner = Owner::Value {
                task,
                invocation,
                slot: Slot::Scratch(index),
            };
            vec![
                ScratchEvent::Begin {
                    owner,
                    bytes: scratch,
                },
                ScratchEvent::End { owner },
            ]
        };
        let make = || {
            Program::new(vec![JournalEvent {
                inputs: inputs.clone(),
                scratch: scratch_events.clone(),
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
        let room = self.memory.then(|| {
            self.ceiling
                .saturating_sub(self.register_bytes)
                .saturating_sub(self.arena.held())
                .saturating_sub(self.transient)
        });
        let prepare = native_storage::NativeStorage {
            contract: contract.clone(),
            function,
            args: args.to_vec(),
            output,
            at: prefix.start.at.clone(),
            release,
            ledger: std::rc::Rc::clone(&store),
            heap: self.heap,
            charging: self.charging,
            limits: self.limits,
            room,
            known: answer.map(|(value, _)| (value, 0)),
        };
        let result = &mut frame.regs[site.reg as usize];
        let published = self.publish_source_prepared(
            frame.state.callee,
            activation,
            prefix,
            make,
            prepare,
            |arena, _, edits, storage| {
                debug_assert!(edits.is_empty());
                *result = storage.value();
                storage.publish(arena);
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            if prepared {
                crate::machine::islands::witness::prepared_native_published();
            } else if collection {
                crate::machine::islands::witness::native_collection_published(
                    frame.regs[site.reg as usize],
                );
            } else if text {
                crate::machine::islands::witness::native_text_published();
            } else {
                crate::machine::islands::witness::native_read_published();
            }
        }
        Ok(if published {
            ValuePublication::Published(
                answer.map_or(frame.regs[site.reg as usize], |(value, _)| value),
            )
        } else {
            ValuePublication::Declined
        })
    }

    /// A proven native commit with a scalar answer over scalar operands joins
    /// the open region. Its checked journal releases the operands and holds
    /// the answer; its shared admission is the call's peak above the region's
    /// envelope, the units the checked storage would charge, the close and
    /// the helper depth. `None` leaves the call to the checked path, which
    /// decides every refusal exactly.
    #[allow(clippy::too_many_arguments)]
    fn join_native(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        args: &[Value],
        contract: &doc::NativeResourceContract,
        input_sites: &[lash_kernel_value::sites::Site],
        at: &Point,
        value: Value,
    ) -> Result<Option<ValuePublication>, HeapFault> {
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let Some(activation) = frame.state.source.as_mut() else {
            return Ok(None);
        };
        let Some(frontier) = activation.frontier.as_mut() else {
            return Ok(None);
        };
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("native read owners failed verification"))?;
        let invocation = activation.invocation.id();
        let root = |at: &lash_kernel_value::sites::Site| {
            slots.root(&room::Owner::Operand(at.clone()), self.task, invocation)
        };
        let Some(output) = root(&at.site) else {
            return Ok(None);
        };
        let mut inputs = Vec::with_capacity(input_sites.len());
        for at in input_sites {
            let Some(input) = root(at) else {
                return Ok(None);
            };
            if !self.logical.holds(input, self.heap.pages())? {
                return Ok(Some(ValuePublication::Unavailable));
            }
            inputs.push(input);
        }
        if self.logical.holds(output, self.heap.pages())? {
            return Ok(Some(ValuePublication::Declined));
        }
        // The checked storage replaces the kernel's units by the measured
        // formula over the answer; scalars measure alike in either arena.
        let units = if self.charging {
            let Some(units) = crate::machine::islands::cx::measured_formula(
                self.arena,
                &self.exe.lib(site.lib).charge,
                args,
                Some(&value),
            ) else {
                return Ok(None);
            };
            units
        } else {
            0
        };
        let close = self
            .accounting
            .then_some(site.close)
            .flatten()
            .map_or(0, |close| u64::from(close.total));
        let need = match site.depth {
            Depth::None => 0,
            Depth::Operation(extra) if extra <= 1 => 1,
            Depth::Operation(extra) | Depth::Template(extra) => u64::from(extra) + 1,
        };
        let depth = (self.base + u64::from(self.depth)).saturating_sub(1);
        let Some(end) = self
            .charged
            .checked_add(units)
            .and_then(|end| end.checked_add(close))
        else {
            return Ok(None);
        };
        // Either the call's envelope or the answer's holding prices the
        // event's peak; the larger bounds both.
        let Some(peak) = contract.peak_extra_bytes() else {
            return Ok(None);
        };
        let peak = peak.max(Holding::from_native(self.arena, self.heap.pages(), &value)?.bytes());
        if end > self.bound
            || depth
                .checked_add(need)
                .is_none_or(|depth| depth > self.call_depth)
            || !self
                .logical
                .admit(self.arena, self.heap, peak, self.memory_bound)?
        {
            return Ok(None);
        }
        frontier.proven()?;
        for input in inputs {
            self.logical.replace(input, None);
        }
        self.logical
            .replace(output, Some(SourceValue::Value(value)));
        self.charged = end;
        frame.regs[site.reg as usize] = value;
        Ok(Some(ValuePublication::Published(value)))
    }
}
