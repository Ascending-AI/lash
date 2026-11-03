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
            Opcode::ScopePrefix => {
                let site = &code.sites.scope_prefix[packet.x as usize];
                if !self.fits(site.ahead) {
                    return Err(self.at(site.miss));
                }
            }
            Opcode::CollectionAppend => {
                if let Some(answer) =
                    self.append_collection(frame, &code.sites.collection_append[packet.x as usize])
                {
                    return answer;
                }
            }
            Opcode::CollectionReduce => {
                if let Some(answer) =
                    self.reduce_collection(frame, &code.sites.collection_reduce[packet.x as usize])
                {
                    return answer;
                }
            }
            Opcode::CollectionPlacement => {
                if let Some(answer) = self
                    .place_collection(frame, &code.sites.collection_placement[packet.x as usize])
                {
                    return answer;
                }
            }
            Opcode::CollectionVisit => {
                if let Some(answer) =
                    self.visit_collection(frame, &code.sites.collection_visit[packet.x as usize])
                {
                    return answer;
                }
            }
            Opcode::CollectionReadSlot => {
                if let Some(answer) =
                    self.read_collection(frame, &code.sites.collection_read[packet.x as usize])
                {
                    return answer;
                }
            }
            Opcode::CollectionMerge => {
                if let Some(answer) =
                    self.merge_collection(frame, &code.sites.collection_merge[packet.x as usize])
                {
                    return answer;
                }
            }
            Opcode::CollectionMergeTail => {
                if let Some(answer) = self
                    .merge_collection_tail(frame, &code.sites.collection_tail[packet.x as usize])
                {
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

                frame
                    .state
                    .texts
                    .retain(|token| !texts.contains(&token.physical()));
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
                frame
                    .state
                    .texts
                    .retain(|token| !texts.contains(&token.physical()));
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
                    entry,
                    repeat,
                    id,
                    cost,
                    metered,
                    at,
                    otherwise,
                    ..
                } = &code.sites.header[packet.x as usize];

                if self.deferred_end.is_some() {
                    self.progressed();
                }
                if at.is_some() {
                    if let Some(entry) = entry
                        && self
                            .publish_loop_entry(frame, entry, None, *otherwise)
                            .map_err(|e| self.halt(e.into()))?
                    {
                        return Ok(Next::Continue);
                    }
                    self.scratch.exit.stands = Some(Stands::Iteration { stmt: *id });
                    return Err(Stop);
                }
                // No header fact is committed until the complete charge
                // prefix and (for a cursor) the resident next item fit.
                // A refusal resumes next_iteration in this same step.
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
                // Canonical iteration debts include their entered header.
                // The admitted prefix leaves room for the next unpaid header
                // so an interior stop always retains an exact source point.
                let repeated = if !OBSERVING
                    && self.exe.charging()
                    && let Some(site) = repeat
                {
                    self.repeat_scope(frame, site, *cost, *metered)?
                } else {
                    0
                };
                self.formula(*cost);
                if *metered != 0 {
                    self.scheduled(*metered);
                }
                if repeated != 0
                    && let Some(Position::Loop { started, .. }) = frame.state.control.last_mut()
                {
                    *started = started
                        .checked_add(repeated)
                        .ok_or_else(|| self.halt(fault("loop occurrence overflow")))?;
                }
                self.covered = false;
                frame.state.statement = Some(*id);
            }
            Opcode::LoopBranch => {
                let sites::LoopBranch {
                    entry,
                    reg,
                    pc,
                    otherwise,
                } = &code.sites.loop_branch[packet.x as usize];

                self.transient = self
                    .transient
                    .saturating_sub(std::mem::take(&mut frame.state.transient));
                let value = regs[*reg as usize];
                if let Some(entry) = entry
                    && self
                        .publish_loop_entry(frame, entry, Some(value), *otherwise)
                        .map_err(|e| self.halt(e.into()))?
                {
                    return Ok(Next::Continue);
                }
                return Err(self.at(*pc));
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
                let mut values = [Value::Absent; 3];
                let given = match inputs {
                    OperatorInputs::Registers(args) => {
                        for (to, reg) in values.iter_mut().zip(args.iter()) {
                            *to = frame.regs[*reg as usize];
                        }
                        args.len()
                    }
                    OperatorInputs::Source(reads) => {
                        self.source_inputs(frame, reads, &mut values)?;
                        reads.len()
                    }
                    OperatorInputs::Scalar(_) => unreachable!("scalar inputs returned above"),
                };
                return self.operator_destination(frame, site, &values[..given], *covered, to);
            }
            Opcode::TextWrite => {
                use super::super::code::NativeInputs;
                let sites::TextWrite {
                    site,
                    text_inputs,
                    write,
                } = &code.sites.text_write[packet.x as usize];
                let mut values = [Value::Absent; MAX_ARGS];
                let count = match text_inputs {
                    NativeInputs::Registers(args) => {
                        for (value, reg) in values.iter_mut().zip(args.iter()) {
                            *value = frame.regs[*reg as usize];
                        }
                        args.len()
                    }
                    NativeInputs::Source(reads) => {
                        self.source_inputs(frame, reads, &mut values)?;
                        reads.len()
                    }
                };
                if self
                    .publish_text_write(frame, site, write, &values[..count])
                    .map_err(|error| self.halt(error.into()))?
                    == Some(true)
                {
                    return self.finish_statement(frame, write.leaves, None);
                }
                if frame
                    .state
                    .source
                    .as_ref()
                    .is_none_or(|source| source.original.is_none())
                    && let Some(close) = &site.close
                    && !self.close(close)
                {
                    return Err(self.at_operands(close.head, site.reg, &values[..count]));
                }
                return Err(self.at_operands(site.pc, site.reg, &values[..count]));
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
                    self.source_inputs(frame, reads, &mut values)?;
                    reads.len()
                };
                match self
                    .publish_native_value(frame, site, &values[..given])
                    .map_err(|error| self.halt(error.into()))?
                {
                    source::ValuePublication::Published(value) => {
                        frame.regs[site.reg as usize] = value;
                        return Ok(Next::Continue);
                    }
                    source::ValuePublication::Declined | source::ValuePublication::Unavailable => {
                        if frame
                            .state
                            .source
                            .as_ref()
                            .is_some_and(|source| source.original.is_some())
                        {
                            return Err(self.at_operands(site.pc, site.reg, &values[..given]));
                        }
                        if let Some(close) = &site.close
                            && !self.close(close)
                        {
                            return Err(self.at_operands(close.head, site.reg, &values[..given]));
                        }
                        return Err(self.at_operands(site.pc, site.reg, &values[..given]));
                    }
                }
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
                return self.construct_record(frame, &code.sites.record[packet.x as usize]);
            }
            Opcode::Closure => {
                return self.construct_closure(frame, &code.sites.closure[packet.x as usize]);
            }
            Opcode::Store => {
                let sites::Store {
                    target,
                    index,
                    value,
                    field,
                    pc,
                    inputs,
                    finish,
                } = &code.sites.store[packet.x as usize];

                let values = match inputs {
                    Some(inputs) => {
                        let mut values = [Value::Absent; 3];
                        self.source_inputs(frame, &inputs.reads, &mut values)?;
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
                let published = if let Some(field) = field {
                    self.publish_store(
                        frame,
                        *pc,
                        Some(*field),
                        &[(*target, ordered[0]), (*value, ordered[2])],
                    )
                } else if let Some(index) = index {
                    self.publish_store(
                        frame,
                        *pc,
                        None,
                        &[
                            (*target, ordered[0]),
                            (*index, ordered[1]),
                            (*value, ordered[2]),
                        ],
                    )
                } else {
                    Ok(false)
                }
                .map_err(|error| self.halt(error.into()))?;
                if !published {
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

                let operands = [
                    (*target, regs[*target as usize]),
                    (*index, regs[*index as usize]),
                ];
                if !self
                    .publish_remove(frame, *pc, operands)
                    .map_err(|error| self.halt(error.into()))?
                {
                    let stop = self.at(*pc);
                    for (reg, value) in operands {
                        if !self.scratch.exit.regs.iter().any(|(held, _)| *held == reg) {
                            self.scratch.exit.regs.push((reg, value));
                        }
                    }
                    return Err(stop);
                }
            }
            Opcode::Unit => {
                let sites::Unit { site, src } = &code.sites.unit[packet.x as usize];

                let input = regs[*src as usize];
                let mut published = false;
                if site.source.is_some()
                    && !matches!(input, Value::Tuple(_))
                    && self.projection().is_none()
                {
                    published = self
                        .publish_unit_comparison(frame, site, input)
                        .map_err(|error| self.halt(error.into()))?;
                }
                frame.regs[site.reg as usize] = if published {
                    Value::Bool(false)
                } else {
                    return Err(self.at_held(site.miss, site.reg, input));
                };
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

                let before = self.charged;
                if !self.defer_original_close(frame, close)? {
                    if !self.close_in(close, *covered) {
                        return Err(self.at(close.head));
                    }
                    if let Some(id) = frame.state.statement
                        && let Some(source) = frame.state.source.as_mut()
                    {
                        source.loop_input =
                            crate::compile::room::publication::boundary::PaidLoopInput::record(
                                self.exe,
                                frame.state.callee.plan.code,
                                id,
                                *close,
                                before,
                                self.charged,
                            );
                    }
                }
            }
            Opcode::Write => {
                let sites::Write { site } = &code.sites.write[packet.x as usize];

                if let Some(begin) = &site.begin {
                    self.begin(frame, begin.id, begin.index, begin.ahead)?;
                }
                let value = if let Some(reads) = &site.reads {
                    {
                        let mut values = [Value::Absent; 1];
                        self.source_inputs(frame, reads, &mut values)?;
                        values[0]
                    }
                } else {
                    frame.regs[site.reg as usize]
                };
                return self.write_value(frame, site, value);
            }
            Opcode::Publish => {
                let site = &code.sites.publish[packet.x as usize];
                let value = frame.regs[site.reg as usize];
                match self
                    .publish_session_write(frame, site, value)
                    .map_err(|error| self.halt(error.into()))?
                {
                    Some(true) => {}
                    Some(false) => return Err(self.at(site.pc)),
                    None => return Err(self.at_held(site.pc, site.reg, value)),
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

                if let Some(id) = frame.state.statement {
                    let (stream, statement, _) = self.exe.statement(id);
                    let pc = statement.ops.end() - 1;
                    if matches!(stream.ops[pc as usize], lash_kernel_exec::Op::End)
                        && !self
                            .publish_discard(frame, pc)
                            .map_err(|error| self.halt(error.into()))?
                    {
                        return Err(self.at_held(pc, 0, frame.regs[0]));
                    }
                }
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
            Opcode::Certified
            | Opcode::Begin
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
