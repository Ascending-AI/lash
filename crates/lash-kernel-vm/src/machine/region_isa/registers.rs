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
struct Bank<V: Copy> {
    ints: Vec<Cell<i64>>,
    floats: Vec<Cell<f64>>,
    bools: Vec<Cell<bool>>,
    values: Vec<Cell<Option<V>>>,
}
impl<V: Copy> Bank<V> {
    fn new(width: usize) -> Result<Self, RegionError> {
        fn bank<T: Copy + Default>(count: usize) -> Result<Vec<Cell<T>>, RegionError> {
            let mut bank = Vec::new();
            bank.try_reserve_exact(count)
                .map_err(|_| invalid(Refused::Scratch))?;
            bank.resize(count, Cell::default());
            Ok(bank)
        }
        Ok(Self {
            ints: bank(width)?,
            floats: bank(width)?,
            bools: bank(width)?,
            values: bank(width)?,
        })
    }
    fn reserve(&mut self, width: usize) -> Result<(), RegionError> {
        if width <= self.ints.len() {
            return Ok(());
        }
        let count = width.max(self.ints.len().saturating_mul(2));
        fn reserve<T>(bank: &mut Vec<T>, count: usize) -> Result<(), RegionError> {
            bank.try_reserve(count - bank.len())
                .map_err(|_| invalid(Refused::Scratch))
        }
        reserve(&mut self.ints, count)?;
        reserve(&mut self.floats, count)?;
        reserve(&mut self.bools, count)?;
        reserve(&mut self.values, count)?;
        self.ints.resize(count, Cell::default());
        self.floats.resize(count, Cell::default());
        self.bools.resize(count, Cell::default());
        self.values.resize(count, Cell::default());
        Ok(())
    }
}

pub(super) struct Registers<V: Copy> {
    frames: Vec<Bank<V>>,
    /// Session cells are outside the per-frame banks.
    sessions: Vec<Option<V>>,
}
impl<V: Copy> Registers<V> {
    pub(super) fn new(width: usize, sessions: usize) -> Result<Self, RegionError> {
        let mut frames = Vec::new();
        frames
            .try_reserve_exact(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        frames.push(Bank::new(width)?);
        let mut cells = Vec::new();
        cells
            .try_reserve_exact(sessions)
            .map_err(|_| invalid(Refused::Scratch))?;
        cells.resize(sessions, None);
        Ok(Self {
            frames,
            sessions: cells,
        })
    }
    pub(super) fn reserve_frame(&mut self, frame: usize, width: usize) -> Result<(), RegionError> {
        if let Some(bank) = self.frames.get_mut(frame) {
            return bank.reserve(width);
        }
        if frame != self.frames.len() {
            return Err(invalid(Refused::Facts));
        }
        self.frames
            .try_reserve(1)
            .map_err(|_| invalid(Refused::Scratch))?;
        self.frames.push(Bank::new(width)?);
        Ok(())
    }
    fn bank(&self, frame: usize) -> Result<&Bank<V>, RegionError> {
        self.frames
            .get(frame)
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
                self.bank(frame)?.ints.get(index as usize),
                invocation,
            )?)),
            Register::Float(index) => {
                Operand::Immediate(Value::Float(lash_kernel_doc::Float::new(read(
                    self.bank(frame)?.floats.get(index as usize),
                    invocation,
                )?)))
            }
            Register::Bool(index) => Operand::Immediate(Value::Bool(read(
                self.bank(frame)?.bools.get(index as usize),
                invocation,
            )?)),
            Register::Value(index) => Operand::Dynamic(
                read(self.bank(frame)?.values.get(index as usize), invocation)?
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
            .bank(frame)?
            .ints
            .get(index as usize)
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
            .bank(frame)?
            .floats
            .get(index as usize)
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
                let at = index as usize;
                self.bank(frame)?
                    .ints
                    .get(at)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Int(value))
            }
            Register::Float(index) => {
                let Value::Float(value) = value.scalar(view)? else {
                    return Err(invalid(Refused::Facts));
                };
                let at = index as usize;
                self.bank(frame)?
                    .floats
                    .get(at)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Float(value.get()))
            }
            Register::Bool(index) => {
                let Value::Bool(value) = value.scalar(view)? else {
                    return Err(invalid(Refused::Facts));
                };
                let at = index as usize;
                self.bank(frame)?
                    .bools
                    .get(at)
                    .ok_or_else(|| invalid(Refused::Facts))?;
                (at, PreparedValue::Bool(value))
            }
            Register::Value(index) => {
                let value = value.bind(view).map_err(RegionError::Heap)?;
                let at = index as usize;
                self.bank(frame)?
                    .values
                    .get(at)
                    .ok_or_else(|| invalid(Refused::Facts))?;
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
            frame,
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
            PreparedValue::Int(value) => publish(
                &mut self.frames[write.frame].ints[write.at],
                write.invocation,
                value,
            ),
            PreparedValue::Float(value) => publish(
                &mut self.frames[write.frame].floats[write.at],
                write.invocation,
                value,
            ),
            PreparedValue::Bool(value) => publish(
                &mut self.frames[write.frame].bools[write.at],
                write.invocation,
                value,
            ),
            PreparedValue::Dynamic(value) => publish(
                &mut self.frames[write.frame].values[write.at],
                write.invocation,
                Some(value),
            ),
            PreparedValue::Session(value) => self.sessions[write.at] = Some(value),
        }
    }

    pub(super) fn clear(&mut self, frame: usize) -> Result<(), RegionError> {
        let bank = self
            .frames
            .get_mut(frame)
            .ok_or_else(|| invalid(Refused::Facts))?;
        for cell in &mut bank.ints {
            cell.valid = false;
        }
        for cell in &mut bank.floats {
            cell.valid = false;
        }
        for cell in &mut bank.bools {
            cell.valid = false;
        }
        for cell in &mut bank.values {
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
    frame: usize,
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
                let at = index as usize;
                let cell = self
                    .frames
                    .get_mut(frame)
                    .ok_or_else(|| invalid(Refused::Facts))?
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
