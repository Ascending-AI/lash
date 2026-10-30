//! Packet dispatch uses typed immutable metadata and initialized slab ranges.

use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn packet(
        &mut self,
        frame: &mut Values<'_, 'e>,
        packet: Packet,
        recovery: &'e Recovery,
        slots: usize,
    ) -> Result<Next, Stop> {
        let code = &frame.state.callee.compiled.code;
        let regs = &mut frame.regs;
        match packet.opcode {
            Opcode::Collection => {
                let (step, tail) = &code.sites.collection[packet.x as usize];
                if let Some(answer) = self.collection_step(frame, step, *tail) {
                    return answer;
                }
            }
            Opcode::Try => {
                let sites::Try { id, finally, after } = &code.sites.try_[packet.x as usize];

                frame.state.may_clean = true;
                frame.state.control.push(Position::Try {
                    cleanup: Cleanup {
                        id: *id,
                        finally: *finally,
                        after: *after,
                        pending: packet.x as usize,
                    },
                    phase: Phase::Body,
                });
                frame.state.statement = None;
            }
            Opcode::Loop => {
                let sites::Loop { id, header, texts } = &code.sites.loop_[packet.x as usize];

                frame.state.texts.retain(|(slot, _)| !texts.contains(slot));
                frame.state.control.push(Position::Loop {
                    id: *id,
                    header: *header,
                    started: 0,
                    cursor: None,
                    guard: None,
                });
                frame.state.statement = None;
            }
            Opcode::For => {
                let sites::For {
                    id,
                    header,
                    reg,
                    pc,
                    via,
                    texts,
                } = &code.sites.for_[packet.x as usize];

                if *via {
                    return Err(self.at(*pc));
                }
                frame.state.texts.retain(|(slot, _)| !texts.contains(slot));
                let cursor = Cursor::over(regs[*reg as usize], false).map_err(|_| self.at(*pc))?;
                frame.state.control.push(Position::Loop {
                    id: *id,
                    header: *header,
                    started: 0,
                    cursor: Some(cursor),
                    guard: None,
                });
                frame.state.statement = None;
            }
            Opcode::Header => {
                let sites::Header {
                    repeat,
                    id,
                    cost,
                    metered,
                    at,
                    item_hook,
                    otherwise,
                    ..
                } = &code.sites.header[packet.x as usize];

                if self.deferred_end.is_some() {
                    self.progressed();
                }
                // No header fact is committed until the complete charge
                // prefix and (for a cursor) the resident next item fit.
                // A refusal resumes next_iteration in this same step.
                let next = if let Some(at) = at {
                    let Some(Position::Loop {
                        cursor: Some(cursor),
                        guard,
                        ..
                    }) = frame.state.control.last()
                    else {
                        return Err(self.halt(fault("a for header has no cursor")));
                    };
                    if *item_hook || guard.is_some() {
                        self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                        return Err(Stop);
                    }
                    let Some(next) = cursor.resident_step(self.arena) else {
                        self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                        return Err(Stop);
                    };
                    match next {
                        Ok((cursor, value)) => Some((*at, cursor, value)),
                        _ => {
                            self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                            #[cfg(any(test, feature = "perf-witness"))]
                            {
                                self.scratch.exit.reason = Some(Reason::ResidentRead);
                            }
                            return Err(Stop);
                        }
                    }
                } else {
                    None
                };
                if next.as_ref().is_some_and(|(at, _, value)| {
                    value.is_some() && frame.state.callee.plan.shared[*at] && !self.cell_room(1)
                }) {
                    self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                    return Err(Stop);
                }
                if !self.fits((if self.accounting { *cost } else { 0 }).saturating_add(*metered)) {
                    self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                    #[cfg(any(test, feature = "perf-witness"))]
                    {
                        self.scratch.exit.reason = Some(Reason::Charge);
                    }
                    return Err(Stop);
                }
                if at.is_none()
                    && OBSERVING
                    && let Some(watching) = &self.projection()
                {
                    watching.condition(self.exe, *id);
                }
                // The typed kernel executes every iteration and charges
                // the next test last. Reserve the metered tail so a paid
                // test never has to materialize without a source PC.
                let mut repeated = 0;
                if self.admitted
                    && !OBSERVING
                    && self.exe.charging()
                    && let Some(site) = repeat
                {
                    let window = repeat::Window {
                        charged: self.charged + cost,
                        bound: self.bound.saturating_sub(*metered),
                        slice_end: self.slice_end.saturating_sub(*metered),
                        iteration_max: site.iteration_max,
                        loop_test: *cost,
                    };
                    // The first header unit precedes the repeated source
                    // schedule on a tape. If no iteration can run, its normal
                    // header path below records the unit once instead.
                    let pending = self.tape.map(|_| crate::observe::ChargeTape::new());
                    let (iterations, units) = site.run.run(
                        self.exe,
                        frame,
                        window,
                        pending.as_ref(),
                        #[cfg(any(test, feature = "perf-witness"))]
                        &self.scratch.census,
                        self.scratch.counting,
                    );
                    if iterations != 0 {
                        self.formula(*cost);
                        self.charged += units;
                        if let (Some(tape), Some(pending)) = (self.tape, &pending) {
                            for weight in pending.steps() {
                                tape.push(weight);
                            }
                        }
                        if let Some(recorder) = self.recorder {
                            recorder.typed(iterations);
                        }
                        repeated = iterations;
                        #[cfg(test)]
                        super::REPEATED_TEMPLATE.with(|n| n.set(n.get() + iterations));
                    }
                }
                if repeated == 0 {
                    self.formula(*cost);
                }
                if *metered != 0 {
                    self.scheduled(*metered);
                }
                if repeated != 0
                    && let Some(Position::Loop { started, .. }) = frame.state.control.last_mut()
                {
                    *started += repeated;
                }
                self.covered = false;
                if let Some((at, next_cursor, value)) = next {
                    let Some(Position::Loop {
                        cursor, started, ..
                    }) = frame.state.control.last_mut()
                    else {
                        return Err(self.halt(fault("a for header left its loop")));
                    };
                    *cursor = Some(next_cursor);
                    match value {
                        Some(value) => {
                            *started += 1;
                            frame.slots[at] = if frame.state.callee.plan.shared[at] {
                                SlotState::Cell(self.arena.new_cell(value))
                            } else {
                                SlotState::Value(value)
                            };
                        }
                        None => frame.state.pc = *otherwise,
                    }
                } else {
                    frame.state.statement = Some(*id);
                }
            }
            Opcode::LoopBranch => {
                let sites::LoopBranch { reg, pc, otherwise } =
                    &code.sites.loop_branch[packet.x as usize];

                self.transient = self
                    .transient
                    .saturating_sub(std::mem::take(&mut frame.state.transient));
                match regs[*reg as usize] {
                    Value::Bool(true) => {
                        let Some(Position::Loop { started, .. }) = frame.state.control.last_mut()
                        else {
                            return Err(self.halt(fault("a while test left its loop")));
                        };
                        *started += 1;
                    }
                    Value::Bool(false) => frame.state.pc = *otherwise,
                    _ => return Err(self.at(*pc)),
                }
                frame.state.statement = None;
            }
            Opcode::EndLoop => {
                if !matches!(frame.state.control.pop(), Some(Position::Loop { .. })) {
                    return Err(self.halt(fault("an exhausted loop has no control")));
                }
            }
            Opcode::LoopDone => {
                let leaves = &packet.x;

                let Some(Position::Loop { started, .. }) = frame.state.control.pop() else {
                    return Err(self.halt(fault("an exhausted loop has no control")));
                };
                // Only the loop statement's own initial step takes its
                // statement edge. A backedge completes its header step.
                if started == 0 {
                    for _ in 0..*leaves {
                        Self::pop_block(frame);
                    }
                }
                if self.spent() {
                    return Err(Stop);
                }
            }
            Opcode::Depart => {
                let sites::Depart { continuing } = &code.sites.depart[packet.x as usize];

                return self.departure(
                    frame,
                    if *continuing {
                        Departing {
                            how: Departure::Continue,
                            value: Value::Null,
                        }
                    } else {
                        Departing {
                            how: Departure::Break,
                            value: Value::Null,
                        }
                    },
                );
            }
            Opcode::Open => {
                let block = &(packet.x as usize);

                frame.state.control.push(Position::Block(*block, 0));
                frame.state.statement = None;
                if self.spent() || self.arena.boundary_due() {
                    return Err(Stop);
                }
            }
            Opcode::ReturnLiteral => {
                let sites::ReturnLiteral {
                    id,
                    index,
                    ahead,
                    value,
                    close,
                    covered,
                    reg,
                    entry,
                } = &code.sites.return_literal[packet.x as usize];

                if let Some(block) = entry
                    && frame.state.control.is_empty()
                {
                    frame.state.control.push(Position::Block(*block, 0));
                    frame.state.statement = None;
                    if self.spent() {
                        return Err(Stop);
                    }
                }
                self.begin(frame, *id, *index, *ahead)?;
                if let Some(close) = close
                    && !self.close_in(close, *covered)
                {
                    return Err(self.at_held(close.head, *reg, *value));
                }
                if frame.state.recovery
                    || (self.public_return && self.depth == 1 && frame.state.may_clean)
                {
                    return Err(self.at_held(self.exe.stmt(*id).ops.end() - 1, *reg, *value));
                }
                frame.state.returned = Some(ReturnOperand {
                    statement: *id,
                    pc: self.exe.stmt(*id).ops.end() - 1,
                    reg: *reg,
                });
                return self.returning(frame, *value);
            }
            Opcode::Keep => {
                let restart = &code.sites.keep[packet.x as usize].value;

                regs[restart.saved as usize] = regs[restart.reg as usize];
                regs[restart.saved as usize + 1] = Value::Bool(true);
            }

            Opcode::Operator => {
                use super::super::code::{OperatorInputs, ScalarInputs};
                let sites::Operator {
                    site,
                    inputs,
                    covered,
                    to,
                } = &code.sites.operator[packet.x as usize];
                if let OperatorInputs::Scalar(inputs) = inputs {
                    return match inputs {
                        ScalarInputs::Unary(arg) => {
                            self.numeric_unary(frame, site, arg, *covered, to)
                        }
                        ScalarInputs::Binary([left, right]) => {
                            self.numeric_binary(frame, site, (left, right), *covered, to)
                        }
                    };
                }
                if let Destination::Write(write) = to
                    && write.accumulates
                {
                    self.accumulate(frame, write.at);
                }
                let mut values = [Value::Absent; 3];
                let given = match inputs {
                    OperatorInputs::Registers(args) => {
                        for (to, reg) in values.iter_mut().zip(args.iter()) {
                            *to = frame.regs[*reg as usize];
                        }
                        args.len()
                    }
                    OperatorInputs::Source(reads) => {
                        self.inputs(frame.slots, reads, &mut values)?;
                        reads.len()
                    }
                    OperatorInputs::Scalar(_) => unreachable!("scalar inputs returned above"),
                };
                return self.operator_destination(frame, site, &values[..given], *covered, to);
            }
            Opcode::Native => {
                let sites::Native { site, args } = &code.sites.native[packet.x as usize];
                let mut values = [Value::Absent; MAX_ARGS];
                let given = if packet.mode == 0 {
                    for (to, reg) in values.iter_mut().zip(args.iter()) {
                        *to = regs[*reg as usize];
                    }
                    args.len()
                } else {
                    let reads = &code.inputs[packet.y as usize];
                    self.inputs(frame.slots, reads, &mut values)?;
                    reads.len()
                };
                match self
                    .publish_native_read(frame, site, &values[..given])
                    .map_err(|error| self.halt(error.into()))?
                {
                    source::ValuePublication::Published(value) => {
                        frame.regs[site.reg as usize] = value;
                        return Ok(Next::Continue);
                    }
                    source::ValuePublication::Declined => {
                        if let Some(close) = &site.close
                            && !self.close(close)
                        {
                            return Err(self.at_operands(close.head, site.reg, &values[..given]));
                        }
                        return Err(self.at_operands(site.pc, site.reg, &values[..given]));
                    }
                    source::ValuePublication::Unavailable => {}
                }
                let answer = self.retaining(&mut frame.state.transient, |island| {
                    island.native(site, &values, given)
                });
                frame.regs[site.reg as usize] = answer?;
            }
            Opcode::Update => {
                return self.update(frame, &code.sites.update[packet.x as usize].value);
            }
            Opcode::Heap => {
                let sites::Heap {
                    site,
                    args,
                    resident,
                } = &code.sites.heap[packet.x as usize];
                let mut values = [Value::Absent; MAX_ARGS];
                let given = if packet.mode == 0 {
                    for (to, reg) in values.iter_mut().zip(args.iter()) {
                        *to = regs[*reg as usize];
                    }
                    args.len()
                } else {
                    let reads = &code.inputs[packet.y as usize];
                    self.inputs(frame.slots, reads, &mut values)?;
                    reads.len()
                };
                return self.heap_destination(
                    frame,
                    site,
                    args,
                    &values[..given],
                    resident.as_ref(),
                );
            }
            Opcode::Reserve => {
                let sites::Reserve {
                    kind,
                    members,
                    miss,
                } = &code.sites.reserve[packet.x as usize];

                if !self.retaining(&mut frame.state.transient, |island| {
                    island.reserve_construction(*kind, *members)
                }) {
                    return Err(self.at(*miss));
                }
            }
            Opcode::Make => return self.construct(frame, &code.sites.make[packet.x as usize]),
            Opcode::Format => {
                return self.format(frame, &code.sites.format[packet.x as usize].value);
            }
            Opcode::Record => {
                let sites::Record {
                    ahead,
                    names,
                    values,
                    reg,
                    members,
                    miss,
                } = &code.sites.record[packet.x as usize];

                if !self.fits(*ahead) {
                    return Err(self.at(*miss));
                }
                let before = self.transient;
                let floor = self.construction_floor(lash_kernel_doc::ValueKind::Record, *members);
                self.transient = self.transient.saturating_sub(floor);
                frame.state.transient = frame.state.transient.saturating_sub(floor);
                let values = values.iter().map(|reg| regs[*reg as usize]);
                let answer = self.record(names, values);
                frame.state.transient += self.transient.saturating_sub(before);
                let value = answer.ok_or_else(|| self.at(*miss))?;
                regs[*reg as usize] = self.source_result(value)?;
            }
            Opcode::Closure => {
                let sites::Closure { code, reg, miss } = &code.sites.closure[packet.x as usize];

                let closure = self.retaining(&mut frame.state.transient, |island| {
                    island.fresh_closure(frame.state.callee, frame.slots, *code)
                });
                let value = Value::Closure(closure.ok_or_else(|| self.at(*miss))?);
                regs[*reg as usize] = self.source_result(value)?;
            }
            Opcode::Store => {
                let sites::Store {
                    target,
                    index,
                    value,
                    field,
                    pc,
                    inputs,
                    cache,
                    finish,
                } = &code.sites.store[packet.x as usize];

                let values = match inputs {
                    Some(inputs) => {
                        let mut values = [Value::Absent; 3];
                        self.inputs(frame.slots, &inputs.reads, &mut values)?;
                        values
                    }
                    None => [
                        regs[*target as usize],
                        index.map_or(Value::Absent, |reg| regs[reg as usize]),
                        regs[*value as usize],
                    ],
                };
                let ordered = inputs
                    .as_ref()
                    .map_or(values, |inputs| inputs.roles.map(|at| values[at]));
                let statement = cache.then_some(frame.state.statement).flatten();
                let published = if field.is_none()
                    && let Some(index) = index
                {
                    self.publish_list_store(
                        frame,
                        *pc,
                        [
                            (*target, ordered[0]),
                            (*index, ordered[1]),
                            (*value, ordered[2]),
                        ],
                    )
                    .map_err(|error| self.halt(error.into()))?
                } else {
                    false
                };
                let stored = if published {
                    Some(())
                } else {
                    self.retaining(&mut frame.state.transient, |island| {
                        island.store(statement, *field, ordered[0], ordered[1], ordered[2])
                    })
                };
                if stored.is_none() {
                    let stop = self.at(*pc);
                    if let Some(inputs) = inputs {
                        for (read, value) in inputs.reads.iter().zip(values) {
                            if !self
                                .scratch
                                .exit
                                .regs
                                .iter()
                                .any(|(reg, _)| *reg == read.reg())
                            {
                                self.scratch.exit.regs.push((read.reg(), value));
                            }
                        }
                    }
                    return Err(stop);
                }
                if let Some(leaves) = finish {
                    return self.finish_statement(frame, *leaves, None);
                }
            }
            Opcode::Remove => {
                let sites::Remove { target, index, pc } = &code.sites.remove[packet.x as usize];

                self.remove(regs[*target as usize], regs[*index as usize])
                    .ok_or_else(|| self.at(*pc))?;
            }
            Opcode::Unit => {
                let sites::Unit { site, src } = &code.sites.unit[packet.x as usize];

                let input = regs[*src as usize];
                let mut published = false;
                if site.source.is_some()
                    && !matches!(input, Value::Tuple(_))
                    && self.projection().is_none()
                {
                    let answer = Value::Bool(false);
                    if let Some(tuple) = self.empty_tuple() {
                        let units = if self.charging {
                            measured_formula(
                                self.arena,
                                &self.exe.lib(site.lib).charge,
                                &[input, tuple],
                                Some(&answer),
                            )
                        } else {
                            Some(0)
                        };
                        if let Some(units) = units {
                            published = self
                                .publish_unit_comparison(frame, site, input, tuple, units)
                                .map_err(|error| self.halt(error.into()))?;
                            if published && let Some(tape) = self.tape {
                                if let Some(close) = &site.close {
                                    super::super::record(tape, self.stream, close.region);
                                }
                                if units != 0 {
                                    tape.push(Weight::Atomic(units));
                                }
                            }
                        }
                    }
                }
                frame.regs[site.reg as usize] = if published {
                    Value::Bool(false)
                } else {
                    self.retaining(&mut frame.state.transient, |island| {
                        island.unit_test(site, input)
                    })?
                };
            }
            Opcode::Kind => {
                let sites::Kind { site, src } = &code.sites.kind[packet.x as usize];
                let value = if packet.mode == 0 {
                    regs[*src as usize]
                } else {
                    let reads = &code.inputs[packet.y as usize];
                    let mut values = [Value::Absent; 1];
                    self.inputs(frame.slots, reads, &mut values)?;
                    values[0]
                };
                regs[site.same.reg as usize] = self
                    .retaining(&mut frame.state.transient, |island| {
                        island.kind_test(site, value)
                    })?;
            }
            Opcode::Access => {
                let site = &code.sites.access[packet.x as usize].value;

                return self.access(frame, site);
            }
            Opcode::Field => {
                let (dst, field, miss) = (&packet.dst, &(packet.x as usize), &packet.y);

                let x = regs[*dst as usize];
                regs[*dst as usize] = self
                    .field(x, *field)
                    .ok_or_else(|| self.at_held(*miss, *dst, x))?;
            }
            Opcode::Index => {
                let sites::Index { dst, site, miss } = &code.sites.index[packet.x as usize];

                let (x, at) = (regs[*dst as usize], regs[*dst as usize + 1]);
                regs[*dst as usize] = self
                    .retaining(&mut frame.state.transient, |island| {
                        island.item(*site, x, at)
                    })
                    .ok_or_else(|| self.at_operands(*miss, *dst, &[x, at]))?;
            }
            Opcode::Call => {
                let sites::Call { site, read, args } = &code.sites.call[packet.x as usize];

                return self.call(frame, site, (*read, args, &recovery.live), slots);
            }

            Opcode::Cond => {
                let sites::Cond {
                    reg,
                    close,
                    pc,
                    otherwise,
                } = &code.sites.cond[packet.x as usize];

                if let Some(close) = close
                    && !self.close(close)
                {
                    return Err(self.at(close.head));
                }
                match regs[*reg as usize] {
                    Value::Bool(true) => {}
                    Value::Bool(false) => frame.state.pc = *otherwise,
                    _ => return Err(self.at(*pc)),
                }
            }

            Opcode::Close => {
                let sites::Close { close, covered, .. } = &code.sites.close[packet.x as usize];

                if !self.close_in(close, *covered) {
                    return Err(self.at(close.head));
                }
            }
            Opcode::Write => {
                let sites::Write { site } = &code.sites.write[packet.x as usize];

                if let Some(begin) = &site.begin {
                    self.begin(frame, begin.id, begin.index, begin.ahead)?;
                }
                if let Some(answer) = self.access_start(frame, site) {
                    return answer;
                }
                let value = if let Some(reads) = &site.reads {
                    {
                        let mut values = [Value::Absent; 1];
                        self.inputs(frame.slots, reads, &mut values)?;
                        values[0]
                    }
                } else {
                    frame.regs[site.reg as usize]
                };
                return self.write_value(frame, site, value);
            }
            Opcode::Accumulate => {
                let at = &(packet.x as usize);

                self.accumulate(frame, *at);
            }
            Opcode::Publish => {
                let sites::Publish {
                    name,
                    reg,
                    pc,
                    bind,
                } = &code.sites.publish[packet.x as usize];

                if !self.write_session(*name, regs[*reg as usize], *bind) {
                    return Err(self.at(*pc));
                }
            }
            Opcode::Effect => {
                let pc = &packet.x;

                return Err(self.at(*pc));
            }
            Opcode::Return => {
                let (reg, pc) = (&packet.dst, &packet.x);

                // A public root return copies its value out of the run in
                // its source statement, which raises a value that is no data
                // at that statement's coordinate (FIG-7377).
                if frame.state.recovery || (self.public_return && self.depth == 1) {
                    return Err(self.at(*pc));
                }
                let value = regs[*reg as usize];
                let Some(statement) = frame.state.statement else {
                    return Err(self.halt(fault("a return has no source statement")));
                };
                frame.state.returned = Some(ReturnOperand {
                    statement,
                    pc: *pc,
                    reg: *reg,
                });
                return self.returning(frame, value);
            }
            Opcode::Done => {
                let sites::Done { leaves, empty } = &code.sites.done[packet.x as usize];

                return self.finish_statement(frame, *leaves, *empty);
            }
            Opcode::Branch => {
                let sites::Branch {
                    reg,
                    pc,
                    otherwise,
                    opens,
                } = &code.sites.branch[packet.x as usize];

                let arm = match regs[*reg as usize] {
                    Value::Bool(true) => 0,
                    Value::Bool(false) => {
                        frame.state.pc = *otherwise;
                        1
                    }
                    _ => return Err(self.at(*pc)),
                };
                self.source_clear_operands(frame)
                    .and_then(|()| self.logical.flush(self.arena, self.heap))
                    .map_err(|error| self.halt(error.into()))?;
                self.transient = self
                    .transient
                    .saturating_sub(std::mem::take(&mut frame.state.transient));
                frame.state.statement = None;
                if let Some(block) = opens[arm] {
                    frame.state.control.push(Position::Block(block, 0));
                    if self.spent() || self.arena.boundary_due() {
                        #[cfg(any(test, feature = "perf-witness"))]
                        if !self.spent() && self.fits(1) {
                            self.scratch.exit.reason = Some(Reason::SourceContinuation);
                        }
                        return Err(Stop);
                    }
                }
            }
            Opcode::Spawn | Opcode::Yield | Opcode::Join | Opcode::Wait | Opcode::Cancel => {
                let pc = &packet.y;

                // The instruction has paid its ordered operand/charge prefix.
                // Project all windows before a scheduler handler can claim a
                // task, expose an effect, call the host, or collect the arena.
                self.scratch.exit.scheduling = true;
                return Err(self.at(*pc));
            }
            Opcode::Cold => {
                let operation = &code.sites.cold[packet.x as usize].value;

                return self.deferred(frame, operation);
            }
            Opcode::Begin
            | Opcode::Constant
            | Opcode::Local
            | Opcode::Session
            | Opcode::Bind
            | Opcode::Unbind
            | Opcode::Jump => {
                unreachable!("inline packets stay in the execution loop");
            }
        }
        Ok(Next::Continue)
    }
}
