//! One initialized allocation holds every frame's bindings and operands.
//! A frame range is retained until return or source reconstruction. This
//! conservative placement includes all normal, recovery and owner liveness.

use std::ops::{Index, IndexMut, Range};

use super::*;

#[cfg(test)]
mod witness;
#[cfg(test)]
pub(in crate::machine::islands) use witness::StackPlacement;

#[derive(Default)]
pub(in crate::machine::islands) struct Slab {
    values: Vec<SlotState>,
    extent: usize,
    #[cfg(test)]
    calls: u64,
    #[cfg(any(test, feature = "testing"))]
    packets: u64,
    #[cfg(any(test, feature = "testing"))]
    specialized: u64,
    #[cfg(test)]
    placements: Option<Vec<StackPlacement>>,
}

pub(super) struct Ranges {
    pub(super) bindings: Range<usize>,
    pub(super) operands: Range<usize>,
    pub(super) pending: Range<usize>,
}

pub(super) struct Values<'s, 'e> {
    pub(super) state: &'s mut Frame<'e>,
    pub(super) slots: &'s mut [SlotState],
    pub(super) regs: Operands<'s>,
    pub(super) pending: Operands<'s>,
}

/// Operand ranges contain initialized values, never binding cells or holes.
/// Reads name registers already written by the verified source lowering.
pub(in crate::machine::islands) struct Operands<'s>(&'s mut [SlotState]);

impl Index<usize> for Operands<'_> {
    type Output = Value;

    #[inline(always)]
    fn index(&self, at: usize) -> &Value {
        let SlotState::Value(value) = &self.0[at] else {
            unreachable!("an operand range contains only initialized values");
        };
        value
    }
}

impl IndexMut<usize> for Operands<'_> {
    #[inline(always)]
    fn index_mut(&mut self, at: usize) -> &mut Value {
        let SlotState::Value(value) = &mut self.0[at] else {
            unreachable!("an operand range contains only initialized values");
        };
        value
    }
}

impl Values<'_, '_> {
    #[inline(always)]
    pub(super) fn value(&self, at: u32) -> Option<Value> {
        if self.state.callee.plan.captures.contains(&at) {
            return None;
        }
        match self.slots.get(at as usize)? {
            SlotState::Value(value) => Some(*value),
            SlotState::Empty | SlotState::Cell(_) => None,
        }
    }

    #[inline(always)]
    pub(super) fn assign(&mut self, at: u32, value: Value) {
        self.slots[at as usize] = SlotState::Value(value);
    }
}

impl Slab {
    #[cfg(any(test, feature = "testing"))]
    pub(super) fn dispatched(&mut self, packet: Packet, code: &super::super::code::Code) {
        self.packets += 1;
        let specialized = match packet.opcode {
            Opcode::Format
            | Opcode::Update
            | Opcode::Access
            | Opcode::Collection
            | Opcode::Unary
            | Opcode::Binary
            | Opcode::ReturnLiteral
            | Opcode::Kind => true,
            Opcode::Operator => matches!(
                code.sites.operator[packet.x as usize].to,
                Destination::Write(_)
            ),
            Opcode::Write => code.sites.write[packet.x as usize].site.access.is_some(),
            Opcode::Header => code.sites.header[packet.x as usize].repeat.is_some(),
            _ => false,
        };
        self.specialized += u64::from(specialized);
    }

    #[cfg(any(test, feature = "testing"))]
    pub(in crate::machine::islands) fn take_packets(&mut self) -> (u64, u64) {
        (
            std::mem::take(&mut self.packets),
            std::mem::take(&mut self.specialized),
        )
    }

    #[cfg(test)]
    pub(in crate::machine::islands) fn take_calls(&mut self) -> u64 {
        std::mem::take(&mut self.calls)
    }

    #[cfg(test)]
    pub(super) fn entered_call(&mut self) {
        self.calls += 1;
    }

    #[cfg(test)]
    pub(in crate::machine::islands) fn released(&self) -> bool {
        self.extent == 0
            && self.values.iter().all(|slot| match slot {
                SlotState::Empty => true,
                SlotState::Value(value) => value.handle().is_none(),
                SlotState::Cell(_) => false,
            })
    }

    pub(super) fn open(&mut self, bindings: usize, operands: usize, pending: usize) -> Ranges {
        let start = self.extent;
        // Entry admission bounds the total live bindings and operands by
        // ISLAND_SLOTS and ISLAND_HOLD before a frame reaches this allocator.
        let middle = start + bindings;
        let operand_end = middle + operands;
        let end = operand_end + pending;
        self.values
            .resize(self.values.len().max(end), SlotState::Empty);
        // A reused frame has fresh missing bindings and operand roles. No
        // previous activation supplies an absent argument or a recovery value.
        self.values[start..middle].fill(SlotState::Empty);
        self.values[middle..end].fill(SlotState::Value(Value::Null));
        self.extent = end;
        Ranges {
            bindings: start..middle,
            operands: middle..operand_end,
            pending: operand_end..end,
        }
    }

    pub(super) fn call(
        &mut self,
        caller: &Frame<'_>,
        callee: Callee<'_>,
        args: &[u32],
        captured: &[(Value, ArenaHandle)],
    ) -> Ranges {
        #[cfg(test)]
        {
            self.calls += 1;
        }
        let ranges = self.open(
            callee.plan.slots,
            callee.compiled.registers,
            callee.compiled.pending_slots(),
        );
        let (held, fresh) = self.values.split_at_mut(ranges.bindings.start);
        let source = &held[caller.operands.clone()];
        callee.compiled.layout.fill(
            &mut fresh[..ranges.bindings.len()],
            args.iter().map(|reg| {
                let SlotState::Value(value) = source[*reg as usize] else {
                    unreachable!("a call argument is an initialized operand");
                };
                value
            }),
            captured,
        );
        ranges
    }

    pub(super) fn view<'s, 'e>(&'s mut self, state: &'s mut Frame<'e>) -> Values<'s, 'e> {
        Self::view_at(&mut self.values, 0, state)
    }

    fn view_at<'s, 'e>(
        values: &'s mut [SlotState],
        offset: usize,
        state: &'s mut Frame<'e>,
    ) -> Values<'s, 'e> {
        let (bindings, operands) = values.split_at_mut(state.operands.start - offset);
        let slots = &mut bindings[state.bindings.start - offset..state.bindings.end - offset];
        let (regs, pending) = operands.split_at_mut(state.operands.len());
        let regs = Operands(regs);
        let pending = Operands(&mut pending[..state.pending.len()]);
        Values {
            state,
            slots,
            regs,
            pending,
        }
    }

    pub(super) fn pair<'s, 'e>(
        &'s mut self,
        caller: &'s mut Frame<'e>,
        active: &'s mut Frame<'e>,
    ) -> (Values<'s, 'e>, Values<'s, 'e>) {
        let offset = active.bindings.start;
        let (earlier, current) = self.values.split_at_mut(offset);
        (
            Self::view_at(earlier, 0, caller),
            Self::view_at(current, offset, active),
        )
    }

    pub(super) fn clear(&mut self) {
        self.values[..self.extent].fill(SlotState::Empty);
        self.extent = 0;
    }

    pub(super) fn release(&mut self, frame: &Frame<'_>) {
        assert_eq!(
            self.extent, frame.pending.end,
            "slab frames leave in stack order"
        );
        self.values[frame.bindings.start..self.extent].fill(SlotState::Empty);
        self.extent = frame.bindings.start;
    }
}
