//! Pure call inputs bind the admitted parameter window directly.

use super::super::compiled::CallSite;
use super::*;

impl<'e, const OBSERVING: bool> Island<'e, OBSERVING> {
    #[inline(always)]
    pub(super) fn call(
        &mut self,
        frame: &mut Values<'_, 'e>,
        site: &'e CallSite,
        (read, args, live): (bool, &'e [u32], &'e [u32]),
        slots: usize,
    ) -> Result<Next, Stop> {
        #[cfg(any(test, feature = "testing"))]
        assert!(
            self.scratch.arguments.is_empty(),
            "a callback must receive no native scratch lease"
        );
        // Borrow the source-sized window through admission and binding.
        // Only a wide call initializes a maximum-arity window; small calls
        // carry their source-sized operands directly.
        let small;
        let triple;
        let mut wide;
        let inputs = if let Some(inputs) = &site.inputs {
            let callee = if let Some(callee) = &inputs.callee {
                Some(
                    self.operand(frame, callee)
                        .inspect_err(|_| self.call_prefix(site, None))?,
                )
            } else {
                None
            };
            let values: &[Value] = match &*inputs.args {
                [] => &[],
                [arg] => {
                    small = [
                        self.operand(frame, arg)
                            .inspect_err(|_| self.call_prefix(site, callee))?,
                        Value::Absent,
                    ];
                    &small[..1]
                }
                [left, right] => {
                    let x = self
                        .operand(frame, left)
                        .inspect_err(|_| self.call_prefix(site, callee))?;
                    let y = self.operand(frame, right).inspect_err(|_| {
                        self.scratch.exit.regs.push((left.reg(), x));
                        self.call_prefix(site, callee);
                    })?;
                    small = [x, y];
                    &small
                }
                [first, second, third] => {
                    let x = self
                        .operand(frame, first)
                        .inspect_err(|_| self.call_prefix(site, callee))?;
                    let y = self.operand(frame, second).inspect_err(|_| {
                        self.scratch.exit.regs.push((first.reg(), x));
                        self.call_prefix(site, callee);
                    })?;
                    let z = self.operand(frame, third).inspect_err(|_| {
                        self.scratch.exit.regs.push((first.reg(), x));
                        self.scratch.exit.regs.push((second.reg(), y));
                        self.call_prefix(site, callee);
                    })?;
                    triple = [x, y, z];
                    &triple
                }
                args => {
                    wide = [Value::Absent; MAX_ARGS];
                    for (index, arg) in args.iter().enumerate() {
                        wide[index] = self.operand(frame, arg).inspect_err(|_| {
                            for (arg, value) in args[..index].iter().zip(&wide) {
                                self.scratch.exit.regs.push((arg.reg(), *value));
                            }
                            self.call_prefix(site, callee);
                        })?;
                    }
                    &wide[..args.len()]
                }
            };
            Some((callee, values))
        } else {
            None
        };
        let answer = (|| {
            // A partly paid expression must settle its remaining source
            // events before entering a callee. Its affine receipt belongs
            // to this running statement, not to a suspended caller.
            if frame
                .state
                .source
                .as_ref()
                .is_some_and(|source| source.original.is_some())
            {
                return Err(self.at(site.pc));
            }
            let callee = inputs.as_ref().map_or_else(
                || read.then(|| frame.regs[site.reg as usize]),
                |values| values.0,
            );
            if let Some(close) = &site.close
                && !self.close(close)
            {
                return Err(self.at(close.head));
            }
            let (before, root_occurrence) = self.root_prefix(site.root)?;
            if !self.root_fits(before, 0, site.root.is_some() && self.accounting) {
                return Err(self.at(site.pc));
            }
            if let Some(lib) = site.library
                && site.checked
            {
                let function = self.exe.lib(lib);
                let fits =
                    function
                        .header
                        .signature
                        .params
                        .iter()
                        .enumerate()
                        .all(|(at, param)| {
                            let value = inputs
                                .as_ref()
                                .map_or_else(
                                    || args.get(at).map(|reg| frame.regs[*reg as usize]),
                                    |input| input.1.get(at).copied(),
                                )
                                .unwrap_or(Value::Absent);
                            (param.optional && matches!(value, Value::Absent))
                                || match (&param.ty, value) {
                                    (lash_kernel_value::Type::List(item), Value::List(list))
                                        if **item == lash_kernel_value::Type::Any =>
                                    {
                                        self.arena.len(list).ok().is_some_and(|len| {
                                            len == 0 || self.limits.value_depth() > 1
                                        })
                                    }
                                    _ => crate::compile::import_result_fits(&param.ty, value),
                                }
                        });
                if !fits || self.arena.any_cold() {
                    return Err(self.at(site.pc));
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
                    && slots + target.plan.slots <= self.slot_room
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
            if let Some((callee, values)) = &inputs {
                if let Some(callee) = callee {
                    frame.regs[site.reg as usize] = *callee;
                }
                for (reg, value) in args.iter().zip(values.iter()) {
                    frame.regs[*reg as usize] = *value;
                }
            }
            let occurrence = if let Some(at) = site.occurrence.or(root_occurrence) {
                Some(match self.publish_call_enter(frame, site, args, at)? {
                    Some(ordinal) => ordinal,
                    None => self.count(at)?,
                })
            } else {
                if let Some(watching) = &self.projection() {
                    if let Some(act) = site.act {
                        watching.action_start(self.exe, &self.stream.nested[act.ix()].site);
                    } else if let Some(stmt) = frame.state.statement {
                        watching.root_call(self.exe, stmt, args.len());
                    }
                }
                None
            };
            self.root_arguments(before);
            // The call owns its frame transition. No resolved callee
            // crosses the instruction result or is decoded by the loop.
            frame.state.continuation = Some(CallContinuation {
                site,
                args,
                live,
                occurrence,
            });
            (frame.state.admitted, frame.state.swift, frame.state.covered) =
                (self.admitted, self.swift, self.covered);
            self.record_entry(
                target,
                Some((frame.state.callee.plan.code, frame.state.statement)),
            );
            frame.state.pending_call = Some((target, site.library.filter(|_| site.checked)));
            Ok(Next::Call)
        })();
        if answer.is_err()
            && let (Some(inputs), Some((callee, values))) = (&site.inputs, inputs)
        {
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
    fn call_prefix(&mut self, site: &CallSite, callee: Option<Value>) {
        if let Some(callee) = callee {
            self.scratch.exit.regs.insert(0, (site.reg, callee));
        }
    }
}
