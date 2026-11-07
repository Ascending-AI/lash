//! Installed scalar kernels execute once; admission installs their answer later.
use super::super::{code::Register, operands::Operand, *};
use super::Running;
use crate::compile::region_isa::{CheckedKind, Pc, RegionOp};
use lash_kernel_state::kernel_internals::logical::{LeaseValue, NativeBatch};

pub(super) struct ScalarAnswer {
    pub(super) write: Option<(Register, Operand<LeaseValue>)>,
    pub(super) next: Pc,
    pub(super) formula: Option<u64>,
    /// An adopted native answer's contract balances: its retained delta,
    /// peak and scratch peak. Its step accepts the adopted graph.
    pub(super) native: Option<(u64, u64, u64)>,
}
impl Running<'_> {
    /// Whether `op` is a plain `for`'s Enter given what it cannot walk: a
    /// value other than a list, a tuple, a map or a set (`K-ITER-001`).
    pub(super) fn unwalkable(&mut self, op: &RegionOp) -> Result<bool, RegionError> {
        let RegionOp::Checked { kernel, args, .. } = op else {
            return Ok(false);
        };
        if !matches!(kernel.kind(), CheckedKind::Phase) {
            return Ok(false);
        }
        let step = self
            .code
            .program
            .step(self.pc)
            .ok_or_else(|| invalid(Refused::Path))?;
        if step.at.phase != crate::compile::room::Phase::Enter {
            return Ok(false);
        }
        let Some(info) = self
            .code
            .program
            .loop_at(&step.at.site)
            .and_then(|index| self.code.program.loop_info(index))
        else {
            return Ok(false);
        };
        if info.while_loop
            || !matches!(
                self.code.exe.stmt(info.statement).form,
                crate::compile::Form::For { via: None, .. }
            )
        {
            return Ok(false);
        }
        let walked = self.value(*args.first().ok_or_else(|| invalid(Refused::Facts))?)?;
        Ok(!matches!(
            self.view.kind(walked).map_err(RegionError::Heap)?,
            lash_kernel_doc::ValueKind::List
                | lash_kernel_doc::ValueKind::Tuple
                | lash_kernel_doc::ValueKind::Map
                | lash_kernel_doc::ValueKind::Set
        ))
    }
}
impl Running<'_> {
    /// The answer the machine computed for the native call `op` makes: the
    /// graph it built is adopted by the lease, charged by the native's
    /// formula over the actual arguments and answer, as the in-place kernel
    /// charges it, and accepted with the call's own phase.
    pub(super) fn delivered_answer(
        &mut self,
        op: &RegionOp,
        batch: NativeBatch,
    ) -> Result<ScalarAnswer, RegionError> {
        let code = Arc::clone(&self.code);
        let (lib, function, args) = code
            .program
            .native_operands(self.pc)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let (dst, next) = match op {
            RegionOp::Checked { dst, next, .. } => {
                (super::bind::physical(&code.program, *dst)?, *next)
            }
            RegionOp::IntBinary { dst, next, .. } => {
                (super::bind::physical(&code.program, *dst)?, *next)
            }
            _ => return Err(invalid(Refused::Facts)),
        };
        let native = code.exe.lib(lib);
        if native.specialization.function != function
            || !answer_kind_fits(&native.header.signature.result, batch.answer())
        {
            return Err(invalid(Refused::Facts));
        }
        self.call_values.clear();
        for reg in args {
            let argument = self.value(*reg)?;
            self.call_values.push(argument);
        }
        while self.call_values.len() < native.arity {
            let absent = self
                .view
                .import(&Value::Absent)
                .map_err(RegionError::Heap)?;
            self.call_values.push(absent);
        }
        let (certified, held) = (batch.certified(), batch.held());
        let (result, _, bodies) = self
            .view
            .adopt(batch, &self.call_values)
            .map_err(storage_fault)?;
        // An uncertified call is priced by what it built: the answer's
        // occurrence and the bodies adopted, at a peak its nursery's
        // temporaries raise.
        let balances = match certified {
            Some(balances) => balances,
            None => {
                let retained = self
                    .view
                    .price(result)
                    .map_err(RegionError::Heap)?
                    .checked_add(bodies)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                (retained, retained.max(held), 0)
            }
        };
        let formula = if self.limits.charge_enabled {
            Some(
                native
                    .charge
                    .lease(&mut self.view, &self.call_values, result)
                    .map_err(storage_fault)?,
            )
        } else {
            None
        };
        Ok(ScalarAnswer {
            write: Some((dst, Operand::Dynamic(result))),
            next,
            formula,
            native: Some(balances),
        })
    }

    /// None delegates to the same instruction's general checked kernel.
    /// It is not permission to re-evaluate an answered operation.
    pub(super) fn scalar_answer(
        &mut self,
        op: &RegionOp,
    ) -> Result<Option<ScalarAnswer>, RegionError> {
        let mut formula = None;
        let (write, next) = match op {
            RegionOp::Constant { dst, value, next } => {
                (Some((*dst, Operand::Immediate(*value))), *next)
            }
            RegionOp::Move { dst, src, next } => (Some((*dst, self.read(*src)?)), *next),
            // The region stops once an uncaught raise is accepted, at its row.
            RegionOp::Raise { dst, src } => (Some((*dst, self.read(*src)?)), self.pc),
            RegionOp::IntBinary {
                kernel,
                lhs,
                rhs,
                dst,
                next,
            } => {
                let a = self.registers.integer(
                    self.frame_index(),
                    self.reached.invocation,
                    lhs.index(),
                )?;
                let b = self.registers.integer(
                    self.frame_index(),
                    self.reached.invocation,
                    rhs.index(),
                )?;
                let Some(value) = kernel.apply(a, b) else {
                    return Ok(None);
                };
                formula = Some(kernel.charge(a, b, value));
                return Ok(Some(ScalarAnswer {
                    write: Some((
                        Register::Int(dst.index()),
                        Operand::Immediate(Value::Int(value)),
                    )),
                    next: *next,
                    formula,
                    native: None,
                }));
            }
            RegionOp::Return { value } => {
                let destination = self
                    .code
                    .flows
                    .get(self.pc.index())
                    .and_then(Option::as_ref)
                    .and_then(|flow| flow.destination)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                return Ok(Some(ScalarAnswer {
                    write: Some((destination, self.read(*value)?)),
                    next: self
                        .code
                        .program
                        .return_successor(self.pc)
                        .ok_or_else(|| invalid(Refused::Path))?,
                    formula: None,
                    native: None,
                }));
            }
            // A delivering action's Enter loads its atoms; the machine
            // performs the action once the step is accepted.
            RegionOp::Boundary { spec } => {
                let action = spec.action().ok_or_else(|| invalid(Refused::Facts))?;
                for load in &action.atoms {
                    let operand = self.atom(&load.src)?;
                    self.write(load.dst, operand)?;
                }
                return Ok(Some(ScalarAnswer {
                    write: None,
                    next: spec.next().ok_or_else(|| invalid(Refused::Facts))?,
                    formula: None,
                    native: None,
                }));
            }
            // The machine served the effect before its step was accepted.
            RegionOp::Effect {
                value,
                departure,
                next,
                ..
            } => {
                return Ok(Some(ScalarAnswer {
                    write: departure
                        .map(|departure| {
                            Ok::<_, RegionError>((
                                super::bind::physical(&self.code.program, departure)?,
                                self.read(*value)?,
                            ))
                        })
                        .transpose()?,
                    next: next.unwrap_or(self.pc),
                    formula: None,
                    native: None,
                }));
            }
            // A `return via`'s Restore moves what its policy gave into the
            // departure the frame returns.
            RegionOp::Exit { .. }
                if self
                    .code
                    .program
                    .step(self.pc)
                    .is_some_and(|step| step.at.phase == crate::compile::room::Phase::Restore) =>
            {
                let step = self
                    .code
                    .program
                    .step(self.pc)
                    .ok_or_else(|| invalid(Refused::Path))?;
                let incoming = self
                    .code
                    .program
                    .recovery(step.before)
                    .and_then(|map| map.graph.incoming())
                    .ok_or_else(|| invalid(Refused::Exit))?;
                let destination = self
                    .code
                    .flows
                    .get(self.pc.index())
                    .and_then(Option::as_ref)
                    .and_then(|flow| flow.destination)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                return Ok(Some(ScalarAnswer {
                    write: Some((destination, self.read(incoming)?)),
                    next: self.pc,
                    formula: None,
                    native: None,
                }));
            }
            RegionOp::Exit { .. } => {
                return Ok(Some(ScalarAnswer {
                    write: None,
                    next: self.pc,
                    formula: None,
                    native: None,
                }));
            }
            RegionOp::Branch { test, yes, no } => {
                let Some(Value::Bool(value)) = self.inline(*test)? else {
                    return Ok(None);
                };
                (None, if value { *yes } else { *no })
            }
            RegionOp::Checked {
                kernel,
                args,
                dst,
                next,
            } => {
                let value = match kernel.kind() {
                    // A plain `for` is entered once its Enter admitted
                    // what it walks, a protocol `for` before `open` gives
                    // what it walks; the loop's state edit is the control's.
                    CheckedKind::LoopControl { stage, .. } => {
                        self.code
                            .program
                            .loop_info(stage.info)
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        return Ok(Some(ScalarAnswer {
                            write: None,
                            next: *next,
                            formula,
                            native: None,
                        }));
                    }
                    CheckedKind::Phase => {
                        // Only the call entering a frame allocates its
                        // parameter cells; no region runs that Enter itself.
                        if self
                            .code
                            .program
                            .step(self.pc)
                            .is_none_or(|step| !step.event.entities.is_empty())
                        {
                            return Err(invalid(Refused::Source));
                        }
                        return Ok(Some(ScalarAnswer {
                            write: None,
                            next: *next,
                            formula,
                            native: None,
                        }));
                    }
                    CheckedKind::Local { cell, .. } => {
                        let register = self
                            .code
                            .flows
                            .get(self.pc.index())
                            .and_then(Option::as_ref)
                            .and_then(|flow| flow.local)
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        let value = self.registers.read(
                            self.frame_index(),
                            self.reached.invocation,
                            register,
                        )?;
                        if *cell {
                            let value = value.bind(&mut self.view).map_err(RegionError::Heap)?;
                            let cell = self.view.object(value).map_err(RegionError::Heap)?;
                            Operand::Dynamic(self.view.read(cell,
                                &lash_kernel_state::kernel_internals::logical::LeaseMember::Variable)
                                .map_err(storage_fault)?)
                        } else {
                            value
                        }
                    }
                    CheckedKind::IntTruth {
                        operation,
                        lhs,
                        rhs,
                        units,
                        ..
                    } => {
                        let a = self.registers.integer(
                            self.frame_index(),
                            self.reached.invocation,
                            lhs.index(),
                        )?;
                        let b = self.registers.integer(
                            self.frame_index(),
                            self.reached.invocation,
                            rhs.index(),
                        )?;
                        let Some(value) = lash_kernel_doc::scalar::int_truth(*operation, a, b)
                        else {
                            return Ok(None);
                        };
                        formula = Some(units[usize::from(a != 0) | (usize::from(b != 0) << 1)]);
                        Operand::Immediate(Value::Bool(value))
                    }
                    CheckedKind::FloatBinary {
                        lhs,
                        rhs,
                        operation,
                        units,
                        ..
                    } => {
                        let a = self.registers.float(
                            self.frame_index(),
                            self.reached.invocation,
                            lhs.index(),
                        )?;
                        let b = self.registers.float(
                            self.frame_index(),
                            self.reached.invocation,
                            rhs.index(),
                        )?;
                        let value = if operation.compares() {
                            let Some(value) = kernel.float_truth(a, b) else {
                                return Ok(None);
                            };
                            Value::Bool(value)
                        } else {
                            let Some(value) = kernel.float_number(a, b) else {
                                return Ok(None);
                            };
                            Value::Float(lash_kernel_doc::Float::new(value))
                        };
                        formula = Some(*units);
                        Operand::Immediate(value)
                    }
                    CheckedKind::PackInt { input } => {
                        Operand::Immediate(Value::Int(self.registers.integer(
                            self.frame_index(),
                            self.reached.invocation,
                            input.index(),
                        )?))
                    }
                    CheckedKind::PackFloat { input } => Operand::Immediate(Value::Float(
                        lash_kernel_doc::Float::new(self.registers.float(
                            self.frame_index(),
                            self.reached.invocation,
                            input.index(),
                        )?),
                    )),
                    CheckedKind::PackBool { input } => {
                        let Some(Value::Bool(value)) = self.inline(*input)? else {
                            return Ok(None);
                        };
                        Operand::Immediate(Value::Bool(value))
                    }
                    CheckedKind::Native { lib, function } => {
                        self.arguments.clear();
                        for reg in args {
                            let Some(value) = self.inline(*reg)? else {
                                return Ok(None);
                            };
                            self.arguments.push(value);
                        }
                        let native = self.code.exe.lib(*lib);
                        if native.specialization.function != *function {
                            return Err(invalid(Refused::Source));
                        }
                        if native.limit.is_some()
                            || native.shares_params
                            || !native.specialization.native.immediate_resources
                        {
                            return Ok(None);
                        }
                        if self.arguments.len() > native.arity {
                            return Ok(None);
                        }
                        while self.arguments.len() < native.arity {
                            self.arguments.push(Value::Absent);
                        }
                        if !native
                            .header
                            .signature
                            .params
                            .iter()
                            .zip(&self.arguments)
                            .all(|(param, value)| {
                                (param.optional && matches!(value, Value::Absent))
                                    || crate::compile::import_result_fits(&param.ty, *value)
                            })
                        {
                            return Ok(None);
                        }
                        let Some((entry, fast)) = &native.specialization.prepared else {
                            return Ok(None);
                        };
                        let Some(answer) =
                            entry.answer(&self.arguments, self.limits.charge_enabled)
                        else {
                            return Ok(None);
                        };
                        if !crate::compile::import_result_fits(
                            &native.header.signature.result,
                            answer.value,
                        ) {
                            return Err(invalid(Refused::Facts));
                        }
                        if self.limits.charge_enabled {
                            formula = match answer
                                .scalar_sizes
                                .and_then(|mask| fast.scalar_units(mask))
                            {
                                Some(units) => Some(units),
                                None => {
                                    self.call_values.clear();
                                    for argument in &self.arguments {
                                        self.call_values.push(
                                            self.view
                                                .import(argument)
                                                .map_err(RegionError::Heap)?,
                                        );
                                    }
                                    let result = self
                                        .view
                                        .import(&answer.value)
                                        .map_err(RegionError::Heap)?;
                                    Some(
                                        native
                                            .charge
                                            .lease(&mut self.view, &self.call_values, result)
                                            .map_err(storage_fault)?,
                                    )
                                }
                            };
                        }
                        Operand::Immediate(answer.value)
                    }
                    _ => return Ok(None),
                };
                (Some((*dst, value)), *next)
            }
            _ => return Ok(None),
        };
        Ok(Some(ScalarAnswer {
            write: write
                .map(|(reg, value)| Ok((super::bind::physical(&self.code.program, reg)?, value)))
                .transpose()?,
            next,
            formula,
            native: None,
        }))
    }
}

/// Whether a native's answer is of the kind its declared result is: the
/// region's facts are of kinds; what a value holds the native answers for.
fn answer_kind_fits(ty: &lash_kernel_value::Type, value: Value) -> bool {
    use lash_kernel_value::Type;
    match ty {
        Type::Any | Type::Json => true,
        Type::Null => matches!(value, Value::Null),
        Type::Absent => matches!(value, Value::Absent),
        Type::Bool => matches!(value, Value::Bool(_)),
        Type::Int => matches!(value, Value::Int(_) | Value::BigInt(_)),
        Type::Float => matches!(value, Value::Float(_)),
        Type::Number => matches!(value, Value::Int(_) | Value::BigInt(_) | Value::Float(_)),
        Type::Text | Type::Enum(_) => matches!(value, Value::Text(_) | Value::LongText(_)),
        Type::Bytes => matches!(value, Value::Bytes(_)),
        Type::Timestamp => matches!(value, Value::Timestamp(_) | Value::BigTimestamp(_)),
        Type::Tuple(_) => matches!(value, Value::Tuple(_)),
        Type::List(_) => matches!(value, Value::List(_)),
        Type::Map(_) => matches!(value, Value::Map(_)),
        Type::Set(_) => matches!(value, Value::Set(_)),
        Type::Record(_) => matches!(value, Value::Record(_)),
        Type::Function(_) => matches!(value, Value::Function(_) | Value::Closure(_)),
        Type::Task(_) => matches!(value, Value::Task(_)),
        Type::Error => matches!(value, Value::Error(_)),
        Type::Handle(_) => matches!(value, Value::Handle(_)),
        Type::Union(members) => members.iter().any(|member| answer_kind_fits(member, value)),
    }
}
