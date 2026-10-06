//! Restoring the read-only projected bindings a resumed execution holds.
//!
//! A projection is plain data on both durable wires (ADR 0132 §9), so a
//! restored value reads through its provider as it did before it parked and
//! nothing inside containers or heap objects needs rebinding. Only the root
//! slots named after the host's projected bindings are re-seeded, exactly as
//! [`SlotState::from_globals`] seeds them for a fresh execution: the host's
//! binding is authoritative for its name, and the slot is read-only again.

use super::*;

impl SlotState {
    /// Re-seeds the root slots named after projected bindings.
    ///
    /// `slot_names` is `None` for a frame whose slots are a function's locals,
    /// which projected bindings never occupy.
    pub(crate) fn rebind_projected_slots(
        &mut self,
        slot_names: Option<&[Name]>,
        bindings: &ProjectedBindings,
    ) {
        let Some(slot_names) = slot_names else {
            return;
        };
        for (index, name) in slot_names.iter().enumerate() {
            let Some(binding) = bindings.get_symbol(&name.symbol) else {
                continue;
            };
            if index < self.values.len() {
                self.values[index] = Some(Value::Projected(binding));
                self.extras.remove_symbol(&name.symbol);
            }
        }
    }
}

impl<'a, H: ExecutionHost> Vm<'a, H> {
    /// Re-seeds every top-level frame's projected-binding slots, once at
    /// resume, before the first instruction runs.
    pub(crate) fn rebind_projected_slots(&mut self, bindings: &ProjectedBindings) {
        let top_level_slot_names = &self.chunk.slot_names;
        self.slots.rebind_projected_slots(
            match self.active_function {
                Some(_) => None,
                None => Some(top_level_slot_names),
            },
            bindings,
        );
        for frame in &mut self.frames {
            frame.slots.rebind_projected_slots(
                match frame.function {
                    Some(_) => None,
                    None => Some(top_level_slot_names),
                },
                bindings,
            );
        }
    }
}
