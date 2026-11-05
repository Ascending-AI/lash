//! Installed scalar kernels execute once; admission installs their answer later.
use super::super::{code::Register, operands::Operand, *};
use super::Running;
use crate::compile::region_isa::{CheckedKind, Pc, RegionOp};
use lash_kernel_state::kernel_internals::logical::LeaseValue;

pub(super) struct ScalarAnswer {
    pub(super) write: Option<(Register, Operand<LeaseValue>)>,
    pub(super) next: Pc,
    pub(super) formula: Option<u64>,
}
impl Running<'_> {
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
            RegionOp::IntBinary {
                kernel,
                lhs,
                rhs,
                dst,
                next,
            } => {
                let a = self
                    .registers
                    .integer(0, self.reached.invocation, lhs.index())?;
                let b = self
                    .registers
                    .integer(0, self.reached.invocation, rhs.index())?;
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
                    CheckedKind::Phase => {
                        return Ok(Some(ScalarAnswer {
                            write: None,
                            next: *next,
                            formula,
                        }));
                    }
                    CheckedKind::Local { .. } => {
                        let register = self
                            .code
                            .flows
                            .get(self.pc.index())
                            .and_then(Option::as_ref)
                            .and_then(|flow| flow.local)
                            .ok_or_else(|| invalid(Refused::Facts))?;
                        self.registers.read(0, self.reached.invocation, register)?
                    }
                    CheckedKind::IntTruth {
                        operation,
                        lhs,
                        rhs,
                        units,
                        ..
                    } => {
                        let a = self
                            .registers
                            .integer(0, self.reached.invocation, lhs.index())?;
                        let b = self
                            .registers
                            .integer(0, self.reached.invocation, rhs.index())?;
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
                        let a = self
                            .registers
                            .float(0, self.reached.invocation, lhs.index())?;
                        let b = self
                            .registers
                            .float(0, self.reached.invocation, rhs.index())?;
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
                    CheckedKind::PackInt { input } => Operand::Immediate(Value::Int(
                        self.registers
                            .integer(0, self.reached.invocation, input.index())?,
                    )),
                    CheckedKind::PackFloat { input } => {
                        Operand::Immediate(Value::Float(lash_kernel_doc::Float::new(
                            self.registers
                                .float(0, self.reached.invocation, input.index())?,
                        )))
                    }
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
                        if native.limit.is_some() || native.shares_params {
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
                            formula = answer.scalar_sizes.and_then(|mask| fast.scalar_units(mask));
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
        }))
    }
}
