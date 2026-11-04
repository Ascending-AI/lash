//! Native results publish their canonical occurrences and logical scratch.
use super::*;
use crate::admission::Allowance;
use crate::machine::source_publication::native_storage;
use lash_kernel_state::kernel_internals::logical::RootValue;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

use crate::machine::islands::compiled;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    fn native_publication_room(&self) -> bool {
        // Canonical source admission owns both quiet and detailed debt. The
        // installed contract and detached preparation decline actual cold
        // inputs; unrelated cold objects cannot exclude this publication.
        !self.tight && self.projection().is_none() && self.storage_room(0)
    }

    pub(in crate::machine::islands::execute) fn publish_native_value(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &NativeSite,
        args: &[Value],
    ) -> Result<ValuePublication, HeapFault> {
        #[cfg(test)]
        if self.scratch.chilled {
            return Ok(ValuePublication::Unavailable);
        }
        let join = matches!(site.direct(self.exe), Some(Direct::TextJoin));
        if join || self.exe.lib(site.lib).kernels(self.exe).concat.is_some() {
            match self.publish_text_value(frame, site, args, join)? {
                ValuePublication::Unavailable => {}
                published => return Ok(published),
            }
        }
        self.publish_native_answer(frame, site, args, false, None)
    }

    /// Prepared operators use the same exact source boundary as native calls.
    /// Their canonical entry computes a value and price before either publishes.
    pub(in crate::machine::islands::execute) fn publish_prepared_value(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &OperatorSite,
        args: &[Value],
    ) -> Result<ValuePublication, HeapFault> {
        #[cfg(test)]
        if self.scratch.chilled {
            return Ok(ValuePublication::Unavailable);
        }
        if !self.native_publication_room()
            || (site.root.is_none() && site.counted.is_some())
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
            charges: site.charges.clone(),
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
        let root_ready = self.publish_root_arguments(frame, site, args)?;
        if !root_ready {
            return Ok(ValuePublication::Unavailable);
        }
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
        if !self.native_publication_room()
            || function.limit.is_some()
            || function.shares_params
            || args.len() > function.arity
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
        let Some(charges) = site.charges.as_deref() else {
            return Ok(ValuePublication::Unavailable);
        };
        let Ok(unpaid) = source.next_after_commit(at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(op) = stream.ops.get(site.pc as usize) else {
            return Ok(ValuePublication::Unavailable);
        };
        let root = matches!(op, lash_kernel_exec::Op::Act);
        let registers = op.reads();
        if !root && registers.len() != args.len() {
            return Ok(ValuePublication::Unavailable);
        }
        let operands = if root {
            args.to_vec()
        } else {
            registers
                .iter()
                .map(|reg| frame.regs[reg.0 as usize])
                .collect()
        };
        let mut arguments = args.to_vec();
        arguments.resize(function.arity, Value::Absent);
        // Check the complete source consumer before evaluating a prepared
        // answer. Unsupported expressions retain their original handler.
        let resident = match (site.direct(self.exe), args.first()) {
            (Some(Direct::Length(kind, units)), Some(value)) if value.kind() == kind => value
                .handle()
                .and_then(|handle| self.arena.len(handle).ok())
                .and_then(|len| i64::try_from(len).ok())
                .map(|len| (Value::Int(len), if self.charging { units } else { 0 })),
            (Some(Direct::Item(_)), _) => self.item_answer(site, args),
            (Some(Direct::Member(_)), _) => self.membership(site, args),
            _ => None,
        };
        let answer =
            resident.or_else(|| {
                let (entry, fast) = function.specialization.prepared.as_ref()?;
                if !self.depth_fits(site.depth)
                    || !function.header.signature.params.iter().zip(&arguments).all(
                        |(param, value)| {
                            (param.optional && matches!(value, Value::Absent))
                                || crate::compile::import_result_fits(&param.ty, *value)
                        },
                    )
                {
                    return None;
                }
                if let Some((value, units)) = kernel {
                    return crate::compile::import_result_fits(
                        &function.header.signature.result,
                        value,
                    )
                    .then_some((value, units));
                }
                let answer = entry.answer(&arguments, self.charging)?;
                if !scalar(&answer.value)
                    || !crate::compile::import_result_fits(
                        &function.header.signature.result,
                        answer.value,
                    )
                {
                    return None;
                }
                self.answered(function, fast, &arguments, answer)
            });
        let prepared = resident.is_none() && answer.is_some();

        let LibRun::Native { native, .. } = &function.run else {
            return Ok(ValuePublication::Unavailable);
        };
        let room::CallSource::Native(function_id) = function.specialization.resource else {
            return Ok(ValuePublication::Unavailable);
        };
        let contract = match native.resource_contract(doc::Args::new(
            &arguments,
            self.arena.objects(),
            &self.arena.semantics(),
        )) {
            Ok(Some(contract)) if native_storage::supports(&contract) => contract,
            Ok(None) => {
                let Some((value, _)) = answer else {
                    return Ok(ValuePublication::Unavailable);
                };
                doc::NativeResourceContract::returned(
                    doc::Args::new(&arguments, self.arena.objects(), &self.arena.semantics()),
                    &value,
                )
                .map_err(publication::native_fault)?
            }
            Err(doc::NativeError::Host(error)) => return Err(error),
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            _ => {
                return Ok(ValuePublication::Unavailable);
            }
        };
        let collection = contract
            .events
            .iter()
            .any(|event| matches!(event, doc::NativeResourceEvent::Entity { .. }));
        let Some(called) = room::CheckedRoom::native(function.specialization.function, &contract)
        else {
            return Ok(ValuePublication::Unavailable);
        };
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
        let input_selectors = if root {
            source
                .native_inputs(&at)
                .map_err(|_| malformed("root native inputs failed verification"))?
        } else {
            let Some(roles) = statement.source_roles_at(stream, site.pc) else {
                return Ok(ValuePublication::Unavailable);
            };
            let Some(inputs) = registers
                .iter()
                .map(|register| {
                    roles
                        .iter()
                        .find(|(reg, _)| reg == register)
                        .map(|(_, at)| room::Owner::Operand((**at).clone()))
                })
                .collect::<Option<Box<[_]>>>()
            else {
                return Ok(ValuePublication::Unavailable);
            };
            inputs
        };
        let mut occurrences = vec![];
        let root_occurrence = if root {
            let crate::compile::Form::Action(action) = &statement.form else {
                return Ok(ValuePublication::Unavailable);
            };
            if action.guest {
                occurrences.push(action.occurrence);
                Some((
                    Point {
                        site: at.site.clone(),
                        phase: SourcePhase::Enter,
                    },
                    self.counters
                        .current(action.occurrence)
                        .ok_or_else(|| malformed("root native has no occurrence"))?,
                ))
            } else {
                None
            }
        } else {
            None
        };
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("native read has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("native read owners failed verification"))?;
        let owner = |selector: &room::Owner| {
            slots
                .root(selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("native read operand has no source owner"))
        };
        let mut inputs = Vec::new();
        for selector in &input_selectors {
            inputs.push(owner(selector)?);
        }
        let output = owner(&room::Owner::Operand(at.site.clone()))?;
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let store = self.logical.clone();
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
        let release = input_selectors;
        let mut facts = LeafExecution(
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
                        (Measure::Release(release.clone()), i128::from(released)),
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
        if root {
            let Some((value, _)) = answer else {
                return Ok(ValuePublication::Unavailable);
            };
            let holding = lash_kernel_state::kernel_internals::logical::Holding::from_native(
                self.arena,
                self.heap.pages(),
                &value,
            )?;
            facts
                .0
                .get_mut(&at)
                .ok_or_else(|| malformed("root native facts disappeared"))?
                .values
                .insert(
                    Measure::Local(room::Owner::Operand(at.site.clone())),
                    i128::from(holding.bytes()),
                );
        }
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
                action: root_occurrence,
            },
        };
        let next = SourcePosition {
            at,
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences,
            before: 0,
            units: if self.accounting {
                answer.map_or(0, |(_, units)| units)
            } else {
                0
            },
            close: self.accounting.then_some(site.close).flatten(),
            strict: true,
            depth: match site.depth {
                Depth::None => 0,
                Depth::Operation(extra) if extra <= 1 => 1,
                Depth::Operation(extra) | Depth::Template(extra) => u64::from(extra) + 1,
            },
            safe_point: self.charged,
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
        let recipe = native_storage::NativeStorage {
            contract: contract.clone(),
            function,
            args: arguments,
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
        let tape = activation
            .original
            .is_none()
            .then_some(self.recorder)
            .flatten();
        let published = self.publish_source_boundary(
            frame.state.callee,
            activation,
            prefix,
            native_storage::SourceNative {
                recipe,
                charges,
                inputs,
                operands,
            },
            |arena, _, edits, prepared| {
                let native_storage::NativePublication { storage, debt, .. } = prepared;
                if let Some(tape) = tape {
                    debt.record(tape);
                }
                debug_assert!(edits.is_empty());
                *result = storage.value();
                storage.publish(arena);
            },
        )?;
        #[cfg(test)]
        if published {
            if resident.is_some() && matches!(site.direct(self.exe), Some(Direct::Item(_))) {
                super::super::super::DIRECT_ITEMS.with(|n| n.set(n.get() + 1));
            }
            native_storage::published(true, &contract, args.len() < function.arity);
        }
        #[cfg(any(test, feature = "testing"))]
        if published {
            if function.text_join() || function.text_append(self.exe).is_some() {
                crate::machine::islands::witness::text_operation_published(function.text_join());
            }
            if collection {
                crate::machine::islands::witness::native_collection_published(
                    frame.regs[site.reg as usize],
                );
            } else if text {
                crate::machine::islands::witness::native_text_published();
            } else if prepared {
                crate::machine::islands::witness::prepared_native_published();
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
