//! Physical banks preserve the verified layout and actual lease provenance.
use super::super::{code::Register, operands::Operand, *};
use super::Running;
use crate::compile::region_isa::{CheckedKernel, CheckedKind, Reg};
use lash_kernel_state::kernel_internals::logical::LeaseValue;

impl Running<'_> {
    pub(super) fn read<T>(&self, reg: Reg<T>) -> Result<Operand<LeaseValue>, RegionError> {
        self.registers.read(
            self.parked.len(),
            self.reached.invocation,
            super::bind::physical(&self.code.program, reg)?,
        )
    }
    pub(super) fn value<T>(&mut self, reg: Reg<T>) -> Result<LeaseValue, RegionError> {
        self.read(reg)?
            .bind(&mut self.view)
            .map_err(RegionError::Heap)
    }
    pub(super) fn write<T>(
        &mut self,
        reg: Reg<T>,
        value: Operand<LeaseValue>,
    ) -> Result<(), RegionError> {
        self.registers.write(
            &mut self.view,
            self.parked.len(),
            self.reached.invocation,
            super::bind::physical(&self.code.program, reg)?,
            value,
        )
    }
    // A non-scalar input is a guard miss, never a malformed heap.
    pub(super) fn inline<T>(&self, reg: Reg<T>) -> Result<Option<Value>, RegionError> {
        match self.read(reg)? {
            Operand::Immediate(value) => Ok(Some(value)),
            Operand::Dynamic(value) => self.view.inline(value).map_err(RegionError::Heap),
        }
    }
    pub(super) fn guard(&mut self, kernel: &CheckedKernel) -> Result<bool, RegionError> {
        let invocation = self.reached.invocation;
        let frame = self.parked.len();
        match kernel.kind() {
            CheckedKind::UnpackPair { inputs, outputs } => {
                let (Some(Value::Int(a)), Some(Value::Int(b))) =
                    (self.inline(inputs[0])?, self.inline(inputs[1])?)
                else {
                    return Ok(false);
                };
                for (output, value) in outputs.iter().zip([a, b]) {
                    self.registers.write(
                        &mut self.view,
                        frame,
                        invocation,
                        Register::Int(output.index()),
                        Operand::Immediate(Value::Int(value)),
                    )?;
                }
            }
            CheckedKind::UnpackFloats { inputs, outputs } => {
                let (Some(Value::Float(a)), Some(Value::Float(b))) =
                    (self.inline(inputs[0])?, self.inline(inputs[1])?)
                else {
                    return Ok(false);
                };
                for (output, value) in outputs.iter().zip([a, b]) {
                    self.registers.write(
                        &mut self.view,
                        frame,
                        invocation,
                        Register::Float(output.index()),
                        Operand::Immediate(Value::Float(value)),
                    )?;
                }
            }
            CheckedKind::Truth { input, output } => {
                let Some(Value::Bool(value)) = self.inline(*input)? else {
                    return Ok(false);
                };
                self.registers.write(
                    &mut self.view,
                    self.parked.len(),
                    invocation,
                    Register::Bool(output.index()),
                    Operand::Immediate(Value::Bool(value)),
                )?;
            }
            CheckedKind::UnpackInt { input, output } => {
                let Some(Value::Int(value)) = self.inline(*input)? else {
                    return Ok(false);
                };
                self.registers.write(
                    &mut self.view,
                    self.parked.len(),
                    invocation,
                    Register::Int(output.index()),
                    Operand::Immediate(Value::Int(value)),
                )?;
            }
            _ => return Err(invalid(Refused::Facts)),
        }
        Ok(true)
    }
}
