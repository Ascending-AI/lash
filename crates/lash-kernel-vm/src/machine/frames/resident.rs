//! A task's frames as a resident refinement reads them (seam I,
//! FIG-6748): what their rows would cost and hold if a projection wrote
//! them now, read where they are. A row that holds what its frame holds is
//! read as it is; a frame that changed since its row was written is priced
//! by what it holds in memory, nothing projected.

use lash_kernel_doc::Value;
use lash_kernel_doc::heap::{HeapFault, PageValue};
use lash_kernel_state::arena::{Arena, Pricing, Reached};
use lash_kernel_state::kernel_internals::resident::frame_values;
use lash_kernel_state::kernel_internals::tree::Cell;
use lash_kernel_state::paged::FrameRecord;

use super::super::{Completion, Control, Frame, SlotState, TryPhase};
use super::{Frames, row};

impl Frame {
    /// What the frame's row would cost if a projection wrote it now
    /// (`FrameRecord::cost`): its bindings, what its loops walk, its
    /// departing payloads, its call's arguments and its continuation's
    /// roles. Where it stands costs nothing.
    fn resident_cost(&self, arena: &Arena) -> u64 {
        let mut cost = 64_u64;
        let mut add = |bytes: u64| cost = cost.saturating_add(bytes);
        for slot in &self.slots.values {
            if let SlotState::Value(value) = slot {
                add(arena.cost_of(value));
            }
        }
        for control in &self.control {
            match control {
                // What it walks holds a tuple's members, nothing else.
                Control::For { cursor, header, .. } => {
                    for value in cursor.walked().into_iter().chain(header.held()) {
                        if let Value::Tuple(_) = value {
                            add(arena.summary_of(&value).cost);
                        }
                    }
                }
                Control::Try {
                    phase:
                        TryPhase::Finally(
                            Completion::Return(value)
                            | Completion::ReturnVia { value, .. }
                            | Completion::Throw(value),
                        ),
                    ..
                } => add(arena.cost_of(value)),
                _ => {}
            }
        }
        if let Some(Ok(value) | Err(value)) = &self.cleaning {
            add(arena.cost_of(value));
        }
        if let Some(call) = &self.library {
            call.args.iter().for_each(|value| add(arena.cost_of(value)));
        }
        if let Some(pending) = self.pending.peek() {
            for role in &pending.roles {
                add(arena.cost_of(&role.value));
            }
        }
        cost
    }
}

impl Frames {
    /// What the frames' rows would cost if a projection wrote them now,
    /// giving `out` what they hold: a row's values by their page forms,
    /// a changed frame's through `arena`'s walk, priced in `pricing`.
    pub(in crate::machine) fn price(
        &self,
        arena: &mut Arena,
        pricing: &mut Pricing,
        out: &mut dyn FnMut(Reached<'_>) -> Result<(), HeapFault>,
    ) -> Result<u64, HeapFault> {
        let rows = self.rows.len();
        let mut cost = self.rows.cost();
        // The rows a projection would write: past the stack it removes
        // them, a changed frame's it writes again.
        let mut written = vec![false; rows as usize];
        for index in self.len..rows {
            cost -= row::<FrameRecord>(&self.rows, index, &self.access)?.cost();
            written[index as usize] = true;
        }
        let mut values = Vec::new();
        for (index, frame) in &self.cache {
            if *index < self.held || (frame.owned && frame.holds_as_projected()) {
                continue;
            }
            if *index < rows {
                cost -= row::<FrameRecord>(&self.rows, *index, &self.access)?.cost();
                written[*index as usize] = true;
            }
            cost = cost.saturating_add(frame.resident_cost(arena));
            values.clear();
            frame.each_value(&mut |value| values.push(*value));
            for value in &values {
                arena.price(pricing, value, out)?;
            }
        }
        for (index, written) in written.into_iter().enumerate() {
            if written {
                continue;
            }
            let frame = row::<FrameRecord>(&self.rows, index as u64, &self.access)?;
            frame_values(&frame, &self.access, &mut |value: &PageValue| {
                out(Reached::Page(value))
            })?;
        }
        Ok(cost)
    }
}
