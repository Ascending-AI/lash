//! Native results publish their canonical occurrences and logical scratch.
mod collection;
mod text;
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{
    JournalEvent, Program, PublicationValue as PV, RootValue, ScratchEvent, Slot,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_native_value(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        args: &[Value],
    ) -> Result<ValuePublication, HeapFault> {
        self.publish_native_answer(frame, site, args, false)
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
        self.publish_native_answer(frame, &native, args, true)
    }

    fn publish_native_answer(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        args: &[Value],
        prepared: bool,
    ) -> Result<ValuePublication, HeapFault> {
        let function = self.exe.lib(site.lib);
        let collection = !prepared
            && matches!(
                function.header.signature.result,
                lash_kernel_value::Type::List(_)
                    | lash_kernel_value::Type::Map(_)
                    | lash_kernel_value::Type::Set(_)
                    | lash_kernel_value::Type::Record(_)
            );
        let text = !prepared
            && (matches!(site.direct(self.exe), Some(Direct::TextJoin))
                || function.header.signature.result == lash_kernel_value::Type::Text);
        if (!prepared
            && !collection
            && !text
            && !matches!(
                site.direct(self.exe),
                Some(Direct::Length(_, _) | Direct::Item(_) | Direct::Member(_))
            ))
            || !self.resident_room(0)
            || self.arena.any_cold()
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
        if !self.source_operand_transfer(frame, at) {
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
            Ok(Some(contract)) if collection && collection::supports(&contract) => contract,
            Ok(Some(contract))
                if !collection
                    && (contract.events.is_empty()
                        || ((text || prepared)
                            && contract.events.iter().all(|event| {
                                matches!(event, doc::NativeResourceEvent::Scratch { .. })
                            })))
                    && contract.exit == doc::NativeResourceExit::Returned
                    && (!(text || prepared)
                        || contract.output.origin == doc::NativeResourceOrigin::Immutable) =>
            {
                contract
            }
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            _ => return Ok(ValuePublication::Unavailable),
        };
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
        if answer.is_none() && !text && !collection {
            return Ok(ValuePublication::Unavailable);
        }
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
        for (at, value) in input_sites.into_iter().zip(args) {
            let owner = root(at)?;
            self.logical
                .replace(owner, Some(SourceValue::Value(*value)));
            inputs.push(owner);
        }
        let output = root(at.site.clone())?;
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
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
            units: if self.accounting {
                answer.map_or(0, |(_, units)| units)
            } else {
                0
            },
            close: self.accounting.then_some(site.close).flatten(),
            strict: false,
            depth: match site.depth {
                Depth::None => 0,
                Depth::Operation(extra) => u64::from(extra.saturating_sub(1)),
                Depth::Template(extra) => u64::from(extra),
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
                outputs: answer
                    .map(|(value, _)| (output, PV::Native(value)))
                    .into_iter()
                    .collect(),
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
        let published = if text {
            let room = self.memory.then(|| {
                self.ceiling
                    .saturating_sub(self.register_bytes)
                    .saturating_sub(self.arena.held())
                    .saturating_sub(self.transient)
            });
            let result = &mut frame.regs[site.reg as usize];
            self.publish_source_prepared(
                frame.state.callee,
                activation,
                prefix,
                make,
                text::TextNative {
                    function,
                    args: args.to_vec(),
                    output,
                    charging: self.charging,
                    limits: self.limits,
                    room,
                },
                |arena, _, edits, storage| {
                    debug_assert!(edits.is_empty());
                    storage.batch.publish(arena);
                    *result = storage.value;
                },
            )?
        } else if collection {
            let room = self.memory.then(|| {
                self.ceiling
                    .saturating_sub(self.register_bytes)
                    .saturating_sub(self.arena.held())
                    .saturating_sub(self.transient)
            });
            let prepare = collection::CollectionNative {
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
            };
            let result = &mut frame.regs[site.reg as usize];
            self.publish_source_prepared(
                frame.state.callee,
                activation,
                prefix,
                make,
                prepare,
                |arena, _, edits, storage| {
                    debug_assert!(edits.is_empty());
                    storage.batch.publish(arena);
                    *result = storage.value;
                },
            )?
        } else {
            self.publish_source(frame.state.callee, activation, prefix, make, |_, edits| {
                debug_assert!(edits.is_empty())
            })?
        };
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
}
