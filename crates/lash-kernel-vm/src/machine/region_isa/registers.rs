use super::operands::Operand;
use super::storage::View;
use super::{code::Register, *};
#[derive(Clone, Copy)]
struct Cell<T> {
    value: T,
    invocation: u64,
    valid: bool,
}
impl<T: Default> Default for Cell<T> {
    fn default() -> Self {
        Self {
            value: T::default(),
            invocation: 0,
            valid: false,
        }
    }
}
pub(super) struct Registers<V: Copy> {
    ints: Vec<Cell<i64>>,
    floats: Vec<Cell<f64>>,
    bools: Vec<Cell<bool>>,
    values: Vec<Cell<Option<V>>>,
    width: usize,
}
impl<V: Copy> Registers<V> {
    pub(super) fn new(width: usize, frames: usize) -> Result<Self, RegionError> {
        let count = width
            .checked_mul(frames)
            .ok_or_else(|| invalid(Refused::Facts))?;
        fn bank<T: Copy + Default>(count: usize) -> Result<Vec<Cell<T>>, RegionError> {
            let mut values = Vec::new();
            values
                .try_reserve_exact(count)
                .map_err(|_| invalid(Refused::Scratch))?;
            values.resize(count, Cell::default());
            Ok(values)
        }
        Ok(Self {
            ints: bank(count)?,
            floats: bank(count)?,
            bools: bank(count)?,
            values: bank(count)?,
            width,
        })
    }
    pub(super) fn reserve_frame(&mut self, frame: usize) -> Result<(), RegionError> {
        let required = frame
            .checked_add(1)
            .and_then(|n| self.width.checked_mul(n))
            .ok_or_else(|| invalid(Refused::Facts))?;
        if required <= self.ints.len() {
            return Ok(());
        }
        let count = required.max(self.ints.len().saturating_mul(2));
        fn grow<T: Copy + Default>(
            bank: &mut Vec<Cell<T>>,
            count: usize,
        ) -> Result<(), RegionError> {
            bank.try_reserve(count - bank.len())
                .map_err(|_| invalid(Refused::Scratch))?;
            bank.resize(count, Cell::default());
            Ok(())
        }
        grow(&mut self.ints, count)?;
        grow(&mut self.floats, count)?;
        grow(&mut self.bools, count)?;
        grow(&mut self.values, count)
    }
    fn at(&self, frame: usize, index: u32) -> Result<usize, RegionError> {
        if index as usize >= self.width {
            return Err(invalid(Refused::Facts));
        }
        frame
            .checked_mul(self.width)
            .and_then(|base| base.checked_add(index as usize))
            .ok_or_else(|| invalid(Refused::Facts))
    }
    pub(super) fn read(
        &self,
        frame: usize,
        invocation: u64,
        reg: Register,
    ) -> Result<Operand<V>, RegionError> {
        fn read<T: Copy>(cell: Option<&Cell<T>>, invocation: u64) -> Result<T, RegionError> {
            let cell = cell.ok_or_else(|| invalid(Refused::Facts))?;
            if !cell.valid || cell.invocation != invocation {
                return Err(invalid(Refused::Facts));
            }
            Ok(cell.value)
        }
        Ok(match reg {
            Register::Int(index) => Operand::Immediate(Value::Int(read(
                self.ints.get(self.at(frame, index)?),
                invocation,
            )?)),
            Register::Float(index) => {
                Operand::Immediate(Value::Float(lash_kernel_doc::Float::new(read(
                    self.floats.get(self.at(frame, index)?),
                    invocation,
                )?)))
            }
            Register::Bool(index) => Operand::Immediate(Value::Bool(read(
                self.bools.get(self.at(frame, index)?),
                invocation,
            )?)),
            Register::Value(index) => Operand::Dynamic(
                read(self.values.get(self.at(frame, index)?), invocation)?
                    .ok_or_else(|| invalid(Refused::Facts))?,
            ),
        })
    }
    pub(super) fn integer(
        &self,
        frame: usize,
        invocation: u64,
        index: u32,
    ) -> Result<i64, RegionError> {
        let cell = self
            .ints
            .get(self.at(frame, index)?)
            .ok_or_else(|| invalid(Refused::Facts))?;
        if !cell.valid || cell.invocation != invocation {
            return Err(invalid(Refused::Facts));
        }
        Ok(cell.value)
    }
    pub(super) fn float(
        &self,
        frame: usize,
        invocation: u64,
        index: u32,
    ) -> Result<f64, RegionError> {
        let cell = self
            .floats
            .get(self.at(frame, index)?)
            .ok_or_else(|| invalid(Refused::Facts))?;
        if !cell.valid || cell.invocation != invocation {
            return Err(invalid(Refused::Facts));
        }
        Ok(cell.value)
    }
    pub(super) fn write<S: View<Value = V>>(
        &mut self,
        view: &mut S,
        frame: usize,
        invocation: u64,
        reg: Register,
        value: Operand<V>,
    ) -> Result<(), RegionError> {
        let write = self.prepare_write(view, frame, invocation, reg, value)?;
        self.publish_write(write);
        Ok(())
    }

    /// Resolve imports, packing and the exact initialized destination before
    /// a source phase accepts. Preparation does not change any register.
    pub(super) fn prepare_write<S: View<Value = V>>(
        &self,
        view: &mut S,
        frame: usize,
        invocation: u64,
        reg: Register,
        value: Operand<V>,
    ) -> Result<PreparedWrite<V>, RegionError> {
        let (at, value) = match reg {
            Register::Int(index) => {
                let Value::Int(value) = value.scalar(view)? else {
                    return Err(invalid(Refused::Facts));
                };
                let at = self.at(frame, index)?;
                self.ints.get(at).ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Int(value))
            }
            Register::Float(index) => {
                let Value::Float(value) = value.scalar(view)? else {
                    return Err(invalid(Refused::Facts));
                };
                let at = self.at(frame, index)?;
                self.floats.get(at).ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Float(value.get()))
            }
            Register::Bool(index) => {
                let Value::Bool(value) = value.scalar(view)? else {
                    return Err(invalid(Refused::Facts));
                };
                let at = self.at(frame, index)?;
                self.bools.get(at).ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Bool(value))
            }
            Register::Value(index) => {
                let value = value.bind(view).map_err(RegionError::Heap)?;
                let at = self.at(frame, index)?;
                self.values.get(at).ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Dynamic(value))
            }
        };
        Ok(PreparedWrite {
            at,
            invocation,
            value,
        })
    }

    /// The destination was checked against this bank before phase acceptance.
    /// Publishing performs no import, conversion, allocation or validation.
    pub(super) fn publish_write(&mut self, write: PreparedWrite<V>) {
        fn publish<T>(cell: &mut Cell<T>, invocation: u64, value: T) {
            *cell = Cell {
                value,
                invocation,
                valid: true,
            };
        }
        match write.value {
            PreparedValue::Int(value) => publish(&mut self.ints[write.at], write.invocation, value),
            PreparedValue::Float(value) => {
                publish(&mut self.floats[write.at], write.invocation, value)
            }
            PreparedValue::Bool(value) => {
                publish(&mut self.bools[write.at], write.invocation, value)
            }
            PreparedValue::Dynamic(value) => {
                publish(&mut self.values[write.at], write.invocation, Some(value))
            }
        }
    }

    pub(super) fn clear(&mut self, frame: usize) -> Result<(), RegionError> {
        let base = frame
            .checked_mul(self.width)
            .ok_or_else(|| invalid(Refused::Facts))?;
        let end = base
            .checked_add(self.width)
            .ok_or_else(|| invalid(Refused::Facts))?;
        for cell in self
            .ints
            .get_mut(base..end)
            .ok_or_else(|| invalid(Refused::Facts))?
        {
            cell.valid = false;
        }
        for cell in self
            .floats
            .get_mut(base..end)
            .ok_or_else(|| invalid(Refused::Facts))?
        {
            cell.valid = false;
        }
        for cell in self
            .bools
            .get_mut(base..end)
            .ok_or_else(|| invalid(Refused::Facts))?
        {
            cell.valid = false;
        }
        for cell in self
            .values
            .get_mut(base..end)
            .ok_or_else(|| invalid(Refused::Facts))?
        {
            cell.valid = false;
            cell.value = None;
        }
        Ok(())
    }
}

enum PreparedValue<V: Copy> {
    Int(i64),
    Float(f64),
    Bool(bool),
    Dynamic(V),
}
pub(super) struct PreparedWrite<V: Copy> {
    at: usize,
    invocation: u64,
    value: PreparedValue<V>,
}

pub(super) struct DynamicWrite<'a, V: Copy> {
    cell: &'a mut Cell<Option<V>>,
    invocation: u64,
}
impl<V: Copy> Registers<V> {
    pub(super) fn prepare_dynamic(
        &mut self,
        frame: usize,
        invocation: u64,
        index: u32,
    ) -> Result<DynamicWrite<'_, V>, RegionError> {
        let at = self.at(frame, index)?;
        let cell = self
            .values
            .get_mut(at)
            .ok_or_else(|| invalid(Refused::Facts))?;
        Ok(DynamicWrite { cell, invocation })
    }
}
impl<V: Copy> DynamicWrite<'_, V> {
    pub(super) fn publish(self, value: Option<V>) {
        *self.cell = Cell {
            value,
            invocation: self.invocation,
            valid: true,
        };
    }
}
