//! Re-binding restored projections to their live host views (FIG-2865).
//!
//! A continuation or snapshot decodes every projection as a placeholder. The
//! walks here run once at resume, before the first instruction, so a projection
//! that parked in a slot, on the operand stack, inside a container or inside a
//! heap object comes back holding the host's view instead.

use super::*;
use crate::runtime::projected_refresh;
use iteration::refresh_iterators;

impl SlotState {
    /// Re-binds restored projections to the live host views, exactly as
    /// [`Self::from_globals`] does for a fresh execution: a slot whose name is a
    /// projected binding becomes that binding again (and read-only again), and
    /// every remaining placeholder anywhere inside the values is refreshed by
    /// its own name (FIG-2865).
    ///
    /// `slot_names` is `None` for a frame whose slots are a function's locals,
    /// which projected bindings never occupy.
    pub(crate) fn refresh_projected(
        &mut self,
        slot_names: Option<&[Name]>,
        bindings: &ProjectedBindings,
    ) {
        if let Some(slot_names) = slot_names {
            for (index, name) in slot_names.iter().enumerate() {
                let Some(live) = bindings.get_symbol(name.symbol) else {
                    continue;
                };
                if index < self.values.len() {
                    self.values[index] = Some(Value::Projected(live));
                    self.extras.remove_symbol(name.symbol);
                }
            }
        }
        projected_refresh::refresh_optional_values(&mut self.values, bindings);
        projected_refresh::refresh_record(&mut self.extras, bindings);
    }
}

impl<'a, H: ExecutionHost> Vm<'a, H> {
    /// Re-binds every restored projection in this VM's reachable state.
    ///
    /// Called once at resume, before the first instruction runs, so a
    /// continuation that parked with a projection in a slot, on the operand
    /// stack, inside a container or inside a heap object comes back holding the
    /// host's live view rather than a placeholder (FIG-2865).
    pub(crate) fn refresh_projected_bindings(&mut self, bindings: &ProjectedBindings) {
        let top_level_slot_names = &self.chunk.slot_names;
        projected_refresh::refresh_values(&mut self.stack, bindings);
        if let Some(value) = self.last_value.as_mut() {
            projected_refresh::refresh_value(value, bindings);
        }
        for entry in self.pending_tools.values_mut().flatten() {
            projected_refresh::refresh_value(entry, bindings);
        }
        self.slots.refresh_projected(
            match self.active_function {
                Some(_) => None,
                None => Some(top_level_slot_names),
            },
            bindings,
        );
        refresh_iterators(&mut self.iter_stack, bindings);
        for frame in &mut self.frames {
            frame.slots.refresh_projected(
                match frame.function {
                    Some(_) => None,
                    None => Some(top_level_slot_names),
                },
                bindings,
            );
            refresh_iterators(&mut frame.iter_stack, bindings);
            match &mut frame.return_target {
                ReturnTarget::Callback(driver) => {
                    projected_refresh::refresh_value(&mut driver.function, bindings);
                    projected_refresh::refresh_values(&mut driver.calls, bindings);
                    projected_refresh::refresh_values(&mut driver.results, bindings);
                }
                ReturnTarget::Coercion(driver) => {
                    projected_refresh::refresh_value(&mut driver.object, bindings);
                }
                ReturnTarget::Direct => {}
            }
        }
        for finally in &mut self.finally_stack {
            if let FinallyCompletion::Throw { value, .. } = &mut finally.completion {
                projected_refresh::refresh_value(value, bindings);
            }
        }
        projected_refresh::refresh_heap(&mut self.heap, bindings);
    }
}
