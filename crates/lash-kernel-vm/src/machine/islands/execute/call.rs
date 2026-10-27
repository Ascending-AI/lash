//! Pure call inputs bind the admitted parameter window directly.

use super::super::compiled::CallSite;
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn call(
        &mut self,
        frame: &mut Frame<'e>,
        site: &CallSite,
        (read, args): (bool, &[u32]),
        callers: &mut Vec<Frame<'e>>,
        slots: &mut usize,
    ) -> Result<Next, Stop> {
        // Borrow the source-sized window through admission and binding.
        // Only a wide call initializes a maximum-arity window; small calls
        // carry their source-sized operands directly.
        let small;
        let triple;
        let mut wide;
        let inputs = if let Some(inputs) = &site.inputs {
            if let Some(held) = inputs.held() {
                frame.regs[held.reg() as usize] = self.operand(frame, held)?;
            }
            let callee = if let Some(callee) = &inputs.callee {
                Some(
                    self.operand(frame, callee)
                        .inspect_err(|_| self.call_prefix(frame, site, None))?,
                )
            } else {
                None
            };
            let values: &[Value] = match &*inputs.args {
                [] => &[],
                [arg] => {
                    small = [
                        self.operand(frame, arg)
                            .inspect_err(|_| self.call_prefix(frame, site, callee))?,
                        Value::Absent,
                    ];
                    &small[..1]
                }
                [left, right] => {
                    let x = self
                        .operand(frame, left)
                        .inspect_err(|_| self.call_prefix(frame, site, callee))?;
                    let y = self.operand(frame, right).inspect_err(|_| {
                        self.scratch.exit.regs.push((left.reg(), x));
                        self.call_prefix(frame, site, callee);
                    })?;
                    small = [x, y];
                    &small
                }
                [first, second, third] if inputs.member().is_none() => {
                    let x = self
                        .operand(frame, first)
                        .inspect_err(|_| self.call_prefix(frame, site, callee))?;
                    let y = self.operand(frame, second).inspect_err(|_| {
                        self.scratch.exit.regs.push((first.reg(), x));
                        self.call_prefix(frame, site, callee);
                    })?;
                    let z = self.operand(frame, third).inspect_err(|_| {
                        self.scratch.exit.regs.push((first.reg(), x));
                        self.scratch.exit.regs.push((second.reg(), y));
                        self.call_prefix(frame, site, callee);
                    })?;
                    triple = [x, y, z];
                    &triple
                }
                args => {
                    if let Some(property) = inputs.member()
                        && let [target, key, value] = args
                        && let Some(values) = self.pure_call_values(frame, [target, key, value])
                        && let Some(answer) =
                            self.member_write(frame.statement, property, site.close, &values)
                    {
                        answer?;
                        frame.regs[site.reg as usize] = values[2];
                        if site.after != 0 {
                            self.admit(site.after);
                        }
                        return Ok(Next::Continue);
                    }
                    wide = [Value::Absent; MAX_ARGS];
                    for (index, arg) in args.iter().enumerate() {
                        wide[index] = self.operand(frame, arg).inspect_err(|_| {
                            for (arg, value) in args[..index].iter().zip(&wide) {
                                self.scratch.exit.regs.push((arg.reg(), *value));
                            }
                            self.call_prefix(frame, site, callee);
                        })?;
                    }
                    &wide[..args.len()]
                }
            };
            Some((callee, values))
        } else {
            None
        };
        let answer =
            (|| {
                let regs = &mut frame.regs;
                let callee = inputs
                    .as_ref()
                    .map_or_else(|| read.then(|| regs[site.reg as usize]), |values| values.0);
                if let Some(close) = &site.close
                    && !self.close(close)
                {
                    return Err(self.at(close.head));
                }
                let (before, root_occurrence) = self.root_prefix(site.root)?;
                if !self.root_fits(before, 0, site.root.is_some() && self.accounting) {
                    return Err(self.at(site.pc));
                }
                if let Some(lib) = site.library {
                    if site.checked {
                        let function = self.exe.lib(lib);
                        let fits = function.header.signature.params.iter().enumerate().all(
                            |(at, param)| {
                                let value = inputs
                                    .as_ref()
                                    .map_or_else(|| regs[args[at] as usize], |input| input.1[at]);
                                (param.optional && matches!(value, Value::Absent))
                                    || match (&param.ty, value) {
                                        (
                                            lash_kernel_value::Type::List(item),
                                            Value::List(list),
                                        ) if **item == lash_kernel_value::Type::Any => {
                                            self.arena.len(list).ok().is_some_and(|len| {
                                                len == 0 || self.limits.value_depth() > 1
                                            })
                                        }
                                        _ => crate::compile::import_result_fits(&param.ty, value),
                                    }
                            },
                        );
                        if !fits || self.arena.any_cold() {
                            return Err(self.at(site.pc));
                        }
                    }
                    if !OBSERVING
                        && self.retaining(&mut frame.transient, |island| {
                            island.call_answer(
                                site,
                                (args, inputs.as_ref().map(|input| input.1)),
                                regs,
                                before,
                                root_occurrence,
                            )
                        })?
                    {
                        return Ok(Next::Continue);
                    }
                }
                let target = match (site.callee, callee) {
                    (Some(code), _) => self.declared(code),
                    (None, Some(callee)) => self.resolve(site.site, callee),
                    _ => None,
                };
                let mut room = true;
                let target = target.filter(|target| {
                    args.len() <= target.compiled.layout.arity
                        && !(site.library.is_some()
                            && self.accounting
                            && (OBSERVING || target.compiled.cleanups))
                        && self.depth < self.frames
                        && *slots + target.plan.slots <= self.slot_room
                        && {
                            room = self.register_room(target.compiled);
                            room
                        }
                });
                let Some(target) = target else {
                    #[cfg(any(test, feature = "perf-witness"))]
                    {
                        self.scratch.exit.reason = Some(if self.depth >= self.frames {
                            Reason::Depth
                        } else if !room {
                            Reason::Capacity
                        } else {
                            Reason::Call
                        });
                    }
                    return Err(self.at(site.pc));
                };
                if OBSERVING
                    && self
                        .watching
                        .as_ref()
                        .is_some_and(|watching| !watching.can_call())
                {
                    return Err(self.at(site.pc));
                }
                if let Some(at) = site.occurrence.or(root_occurrence) {
                    self.count(at)?;
                } else if let Some(watching) = &self.projection() {
                    if let Some(act) = site.act {
                        watching.action_start(self.exe, &self.stream.nested[act.ix()].site);
                    } else if let Some(stmt) = frame.statement {
                        watching.root_call(self.exe, stmt, args.len());
                    }
                }
                self.root_arguments(before);
                // The call owns its frame transition. No resolved callee
                // crosses the instruction result or is decoded by the loop.
                if let Some(value) = self.returned_literal(target) {
                    regs[site.reg as usize] = value;
                    if site.after != 0 {
                        self.admit(site.after);
                    }
                    return Ok(Next::Continue);
                }
                frame.waiting = Some(frame.pc - 1);
                (frame.admitted, frame.swift, frame.covered) =
                    (self.admitted, self.swift, self.covered);
                self.record_entry(target, Some((frame.callee.plan.code, frame.statement)));
                let inner = if let Some((callee, values)) = &inputs {
                    if let Some(callee) = callee {
                        regs[site.reg as usize] = *callee;
                    }
                    for (reg, value) in args.iter().zip(values.iter()) {
                        regs[*reg as usize] = *value;
                    }
                    self.frame_from(target, values[..args.len()].iter().copied())
                } else {
                    self.frame_from(target, args.iter().map(|reg| regs[*reg as usize]))
                };
                *slots += inner.slots.len();
                callers.push(std::mem::replace(frame, inner));
                self.depth += 1;
                self.activate(frame);
                Ok(Next::Continue)
            })();
        if answer.is_err()
            && let (Some(inputs), Some((callee, values))) = (&site.inputs, inputs)
        {
            if let Some(held) = inputs.held() {
                self.scratch
                    .exit
                    .regs
                    .push((held.reg(), frame.regs[held.reg() as usize]));
            }
            if let Some(callee) = callee {
                self.scratch.exit.regs.push((site.reg, callee));
            }
            for (read, value) in inputs.args.iter().zip(values.iter()) {
                self.scratch.exit.regs.push((read.reg(), *value));
            }
        }
        answer
    }
    /// A partial input read keeps the already evaluated source prefix.
    #[cold]
    fn call_prefix(&mut self, frame: &Frame<'e>, site: &CallSite, callee: Option<Value>) {
        if let Some(callee) = callee {
            self.scratch.exit.regs.insert(0, (site.reg, callee));
        }
        if let Some(held) = site.inputs.as_ref().and_then(|inputs| inputs.held()) {
            self.scratch
                .exit
                .regs
                .insert(0, (held.reg(), frame.regs[held.reg() as usize]));
        }
    }
}
