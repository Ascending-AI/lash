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
    /// An atom's operand. A text literal too long to stand inline is
    /// allocated here; the step loading it holds it at its atom role, and
    /// the allocation's own occurrence is released once that step is
    /// accepted ([`Self::release_atoms`]), or when the region halts first.
    pub(super) fn atom(
        &mut self,
        src: &crate::compile::region_isa::AtomSource,
    ) -> Result<Operand<LeaseValue>, RegionError> {
        use crate::compile::region_isa::AtomSource;
        match src {
            AtomSource::Role(src) => self.read(*src),
            AtomSource::Constant(value) => Ok(Operand::Immediate(*value)),
            AtomSource::Text(text) => {
                let phase = self
                    .view
                    .stage(
                        lash_kernel_state::kernel_internals::logical::LeaseOp::Allocate(
                            lash_kernel_state::kernel_internals::logical::LeaseAllocation::text(
                                text,
                            ),
                        ),
                    )
                    .map_err(storage_fault)?;
                let value = phase
                    .reserve()
                    .map_err(RegionError::Heap)?
                    .accept()
                    .ok_or_else(|| invalid(Refused::Facts))?;
                self.scratch
                    .atoms
                    .try_reserve(1)
                    .map_err(|_| invalid(Refused::Scratch))?;
                self.scratch.atoms.push(value);
                Ok(Operand::Dynamic(value))
            }
        }
    }
    /// Releases the allocation occurrences of the long text atoms loaded
    /// for the step just accepted or abandoned.
    pub(super) fn release_atoms(&mut self) -> Result<(), RegionError> {
        while let Some(value) = self.scratch.atoms.pop() {
            let price = self.view.price(value).map_err(RegionError::Heap)?;
            let phase = self
                .view
                .stage(
                    lash_kernel_state::kernel_internals::logical::LeaseOp::Release {
                        value,
                        count: 1,
                    },
                )
                .map_err(storage_fault)?;
            if phase.facts().delta() != -i128::from(price) {
                return Err(invalid(Refused::Facts));
            }
            phase.reserve().map_err(RegionError::Heap)?.accept();
        }
        Ok(())
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
