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
pub(super) struct Registers {
    ints: Vec<Cell<i64>>,
    bools: Vec<Cell<bool>>,
    values: Vec<Cell<Option<Value>>>,
    width: usize,
}
impl Registers {
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
    ) -> Result<Value, RegionError> {
        fn read<T: Copy>(cell: Option<&Cell<T>>, invocation: u64) -> Result<T, RegionError> {
            let cell = cell.ok_or_else(|| invalid(Refused::Facts))?;
            if !cell.valid || cell.invocation != invocation {
                return Err(invalid(Refused::Facts));
            }
            Ok(cell.value)
        }
        Ok(match reg {
            Register::Int(index) => {
                Value::Int(read(self.ints.get(self.at(frame, index)?), invocation)?)
            }
            Register::Bool(index) => {
                Value::Bool(read(self.bools.get(self.at(frame, index)?), invocation)?)
            }
            Register::Value(index) => read(self.values.get(self.at(frame, index)?), invocation)?
                .ok_or_else(|| invalid(Refused::Facts))?,
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
    pub(super) fn write(
        &mut self,
        frame: usize,
        invocation: u64,
        reg: Register,
        value: Value,
    ) -> Result<(), RegionError> {
        fn write<T>(
            cell: Option<&mut Cell<T>>,
            invocation: u64,
            value: T,
        ) -> Result<(), RegionError> {
            let cell = cell.ok_or_else(|| invalid(Refused::Facts))?;
            *cell = Cell {
                value,
                invocation,
                valid: true,
            };
            Ok(())
        }
        match (reg, value) {
            (Register::Int(index), Value::Int(value)) => {
                let at = self.at(frame, index)?;
                write(self.ints.get_mut(at), invocation, value)
            }
            (Register::Bool(index), Value::Bool(value)) => {
                let at = self.at(frame, index)?;
                write(self.bools.get_mut(at), invocation, value)
            }
            (Register::Value(index), value) => {
                let at = self.at(frame, index)?;
                write(self.values.get_mut(at), invocation, Some(value))
            }
            _ => Err(invalid(Refused::Facts)),
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
