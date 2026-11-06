use super::operands::Operand;
use super::storage::View;
use super::{code::Register, *};
#[derive(Clone, Copy)]
pub(super) struct Cell<T> {
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
    /// The lease-wide session bank, by bank cell. Frames neither enter nor
    /// clear it, and a session cell is valid from its import or first
    /// write on, whichever invocation reads it.
    sessions: Vec<Option<V>>,
    width: usize,
}
impl<V: Copy> Registers<V> {
    pub(super) fn new(width: usize, frames: usize, sessions: usize) -> Result<Self, RegionError> {
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
            sessions: {
                let mut cells = Vec::new();
                cells
                    .try_reserve_exact(sessions)
                    .map_err(|_| invalid(Refused::Scratch))?;
                cells.resize(sessions, None);
                cells
            },
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
            Register::Session(bank) => Operand::Dynamic(
                self.sessions
                    .get(bank as usize)
                    .copied()
                    .flatten()
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
            Register::Session(bank) => {
                let value = value.bind(view).map_err(RegionError::Heap)?;
                let at = bank as usize;
                self.sessions
                    .get(at)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Session(value))
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
            PreparedValue::Session(value) => self.sessions[write.at] = Some(value),
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
    Session(V),
}
pub(super) struct PreparedWrite<V: Copy> {
    at: usize,
    invocation: u64,
    value: PreparedValue<V>,
}

pub(super) enum DynamicWrite<'a, V: Copy> {
    Frame {
        cell: &'a mut Cell<Option<V>>,
        invocation: u64,
    },
    Session(&'a mut Option<V>),
}
impl<V: Copy> Registers<V> {
    /// The dynamic destination `reg` names: a frame value register or a
    /// session bank cell.
    pub(super) fn prepare_dynamic(
        &mut self,
        frame: usize,
        invocation: u64,
        reg: Register,
    ) -> Result<DynamicWrite<'_, V>, RegionError> {
        match reg {
            Register::Value(index) => {
                let at = self.at(frame, index)?;
                let cell = self
                    .values
                    .get_mut(at)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                Ok(DynamicWrite::Frame { cell, invocation })
            }
            Register::Session(bank) => self
                .sessions
                .get_mut(bank as usize)
                .map(DynamicWrite::Session)
                .ok_or_else(|| invalid(Refused::Facts)),
            Register::Int(_) | Register::Float(_) | Register::Bool(_) => {
                Err(invalid(Refused::Facts))
            }
        }
    }
}
impl<V: Copy> DynamicWrite<'_, V> {
    pub(super) fn publish(self, value: Option<V>) {
        match self {
            Self::Frame { cell, invocation } => {
                *cell = Cell {
                    value,
                    invocation,
                    valid: true,
                };
            }
            Self::Session(cell) => *cell = value,
        }
    }
}
