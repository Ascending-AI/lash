//! Source native exits publish their detached storage at exact boundaries.
use super::native_storage;
use super::*;
use crate::compile::room::{ExitKind, Measure, Phase, Point, publication::Occurrence};
use crate::compile::{LibId, LibRun};
use crate::machine::{Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_native(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        #[cfg(test)]
        if self.pins.force_cold {
            return Ok(false);
        }
        let (Op::Call { lib, args } | Op::Operator { lib, args }) = running.stream.ops[pc as usize]
        else {
            return Ok(false);
        };
        let lib = LibId(lib.0);
        let function = running.exe.lib(lib);
        let LibRun::Native { native, .. } = &function.run else {
            return Ok(false);
        };
        let room::CallSource::Native(function_id) = function.specialization.resource else {
            return Ok(false);
        };
        if function.limit.is_some() || function.shares_params || args.len as usize > function.arity
        {
            return Ok(false);
        }

        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, code, link, loops) = {
            let frame = self.frame(running.task)?;
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(false);
            };
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.code,
                frame.link,
                loops,
            )
        };
        let Ok(unpaid) = source.next_after_commit(site) else {
            return Ok(false);
        };
        let at = Point {
            site: (**site).clone(),
            phase: Phase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        let Ok(charges) =
            room::charges::ChargeProgram::native_completion(running.exe, code, running.id, pc)
        else {
            return Ok(false);
        };
        self.publish_logical_meter()?;
        let physical_room =
            (self.bounds.memory_meter != crate::MemoryMeter::Off).then(|| self.capacity_headroom());
        // A preparation borrows the register bank. Retain only its immutable
        // values while warming, then reacquire the same unconsumed source roots.
        let operands = {
            let Some(bank) = self.regs.prepare_source_output(running.stream, pc)? else {
                return Ok(false);
            };
            bank.values()
        };
        let mut values = operands.clone();
        values.resize(function.arity, doc::Value::Absent);
        let active = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?
            .frames
            .len()
            - usize::from(running.task == doc::TaskId::MAIN && self.session_cell);
        let known = function
            .specialization
            .prepared
            .as_ref()
            .and_then(|(entry, _)| {
                if active as u64 + u64::from(entry.depth()) > u64::from(self.bounds.call_depth)
                    || !function.header.signature.params.iter().zip(&values).all(
                        |(param, value)| {
                            (param.optional && matches!(value, doc::Value::Absent))
                                || crate::compile::import_result_fits(&param.ty, *value)
                        },
                    )
                {
                    return None;
                }
                let value = match function.scalar_kernel(&self.arena.borrow(), &values) {
                    Some(answer) => answer?,
                    None => entry.answer(&values, self.charging)?.value,
                };
                (crate::compile::scalar(&value)
                    && crate::compile::import_result_fits(&function.header.signature.result, value))
                .then_some((value, entry.depth()))
            });
        let contract = loop {
            let query = {
                let arena = self.arena.borrow();
                let semantics = arena.semantics();
                let args = doc::Args::new(&values, arena.objects(), &semantics);
                match (native.resource_contract(args), known) {
                    (Ok(None), Some((value, _))) => doc::NativeResourceContract::returned(
                        doc::Args::new(&values, arena.objects(), &semantics),
                        &value,
                    )
                    .map(Some),
                    (query, _) => query,
                }
            };
            match query {
                Ok(Some(contract)) if native_storage::supports(&contract) => break contract,
                Err(doc::NativeError::Retry(doc::Retry::Cold { object, chunk })) => {
                    self.warm(object, chunk)?
                }
                Err(doc::NativeError::Host(error)) => return Err(error.into()),
                Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason).into()),
                _ => return Ok(false),
            }
        };
        let Some(bank) = self.regs.prepare_source_output(running.stream, pc)? else {
            return Ok(false);
        };
        if bank
            .values()
            .iter()
            .zip(&operands)
            .any(|(actual, expected)| !actual.same(expected))
        {
            return Err(malformed("native input changed while warming").into());
        }
        let output = bank.owner();
        let inputs = bank.owners();

        let Some(called) = room::CheckedRoom::native(function.specialization.function, &contract)
        else {
            return Ok(false);
        };
        let scratch = contract
            .peak_scratch_bytes()
            .ok_or_else(|| malformed("native scratch contract is invalid"))?;
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("native event disappeared"))?
            .inputs
            .clone();
        let released = self
            .logical
            .clone()
            .borrow()
            .release_cost(&inputs, self.heap.pages())?;
        let recipe = native_storage::NativeStorage {
            contract: contract.clone(),
            function,
            args: values.clone(),
            output,
            at: at.clone(),
            release: release.clone(),
            ledger: self.logical.clone(),
            heap: &self.heap,
            charging: self.charging,
            limits: self.limits,
            known,
            room: physical_room,
        };

        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-native");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        if let Some(observer) = self.observer.as_deref_mut() {
            observer.on(
                crate::observe::Event::NativeEnter {
                    task: running.task,
                    lib,
                },
                self.charged,
                self.arena.borrow().allocations(),
            );
        }
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            - usize::from(running.task == doc::TaskId::MAIN && self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("native has no entered invocation"))?;
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops,
                action: None,
            },
        };
        let next = Position {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
        let mut completion = None;
        let published = Context {
            admission: Admission::Exact(MemoryLimit::of(
                self.bounds.memory_meter,
                &mut self.bounds.memory,
                &mut self.limiters,
            )),
            source: &source,
            link,
            kernel,
            logical: &self.logical,
            heap: &self.heap,
            arena: &mut arena,
            session: &mut self.session,
            resources: SourceResources {
                charged: &mut self.charged,
                counters: &mut task.counters,
                bound: if self.charging {
                    self.bounds.charge_limit
                } else {
                    u64::MAX
                },
                slice: self.slice_end,
                depth,
                limit: u64::from(self.bounds.call_depth),
                safe_point,
            },
            logical_memory: &self.logical_memory,
        }
        .publish_external(
            roots.invocation(),
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at.clone(),
                        LeafFacts {
                            dependencies: values.iter().filter_map(doc::Value::handle).collect(),
                            values: [
                                (Measure::Release(release), i128::from(released)),
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
                ),
                start,
                next,
                allowance: Allowance {
                    occurrences: vec![],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: true,
                    depth: u64::from(function.operation()),
                    safe_point,
                },
            },
            native_storage::SourceNative { recipe, charges: &charges, inputs, operands },
            |arena, _, edits, prepared| {
                debug_assert!(edits.is_empty());
                let native_storage::NativePublication { storage, holding, units, .. } = prepared;
                let value = storage.value();
                completion = Some((
                    units,
                    storage.work(),
                    storage.known(),
                ));
                storage.publish(arena);
                bank.publish(value, holding);
            },
        )
        // Exact source admission is the only memory authority: a native
        // whose result overflows the bound is refused at its own Commit,
        // after its formula is paid, and the refusal names the native.
        .map_err(|halt| match halt {
            Halt::Bound(mut exceeded)
                if exceeded.bound == crate::interface::Bound::Memory
                    && exceeded.function.is_none() =>
            {
                exceeded.function = Some(function.header.name.clone());
                Halt::Bound(exceeded)
            }
            halt => halt,
        })?;
        drop(arena);
        if published {
            let (units, work, known) =
                completion.ok_or_else(|| malformed("native publication has no completion"))?;

            if known {
                self.observe(crate::observe::Event::NativeFast);
            }
            self.observe(crate::observe::Event::GuardWork(work));
            if self.charging {
                self.observe(crate::observe::Event::FormulaEnter(lib));
                self.observe(crate::observe::Event::FormulaExit);
                if units != 0 {
                    self.observe_atomic(units);
                }
            }
        }
        self.observe(crate::observe::Event::NativeExit);
        Ok(published)
    }
}

// The frozen engine's laws: unreachable since the flip; L6 deletes them (FIG-7688).
#[cfg(any())]
mod laws;
