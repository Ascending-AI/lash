//! Physical storage allowances for canonical source operations.
use super::*;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// Claim another physical window only while its bytes and the nursery
    /// fit the same fixed entry allowance, before counting its call.
    #[inline(always)]
    pub(super) fn register_room(&self, compiled: &Compiled) -> bool {
        let bytes = compiled.register_bytes().saturating_add(
            if self.memory && compiled.cell_parameters != 0 {
                self.arena.sequence_bound(1) * compiled.cell_parameters as u64
            } else {
                0
            },
        );
        bytes == 0
            || bytes
                <= self
                    .ceiling
                    .saturating_sub(self.arena.held())
                    .saturating_sub(self.register_bytes)
                    .saturating_sub(self.transient)
    }

    /// The semantic heap context's room test for an operation that has
    /// no formula, temporary work or staged write. Tapes and observations
    /// retain that context as the reference; tight entries decline there.
    // FIG-7510 profiles: retain fixed room bounds in the hot access callers.
    #[inline(always)]
    pub(super) fn resident_room(&self, bytes: u64) -> bool {
        !self.tight
            && self.tape.is_none()
            && self.projection().is_none()
            && self.storage_room(bytes)
    }

    /// Physical storage eligibility for a prepared source operation. Its
    /// accepted source boundary supplies semantic and logical admission.
    pub(super) fn storage_room(&self, bytes: u64) -> bool {
        #[cfg(test)]
        if self.scratch.chilled {
            return false;
        }
        !self.memory
            || bytes
                <= self
                    .ceiling
                    .saturating_sub(self.register_bytes)
                    .saturating_sub(self.arena.held())
                    .saturating_sub(self.transient)
    }

    pub(super) fn construction_floor(
        &self,
        kind: lash_kernel_doc::ValueKind,
        members: usize,
    ) -> u64 {
        if self.memory {
            self.arena.construction_floor(kind, members)
        } else {
            0
        }
    }

    pub(super) fn reserve_construction(
        &mut self,
        kind: lash_kernel_doc::ValueKind,
        members: usize,
    ) -> bool {
        let floor = self.construction_floor(kind, members);
        if floor
            > self
                .ceiling
                .saturating_sub(self.arena.held())
                .saturating_sub(self.register_bytes)
                .saturating_sub(self.transient)
        {
            return false;
        }
        self.transient += floor;
        true
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn cell_room(&self, cells: usize) -> bool {
        !self.memory
            || self.arena.sequence_bound(1).saturating_mul(cells as u64)
                <= self
                    .ceiling
                    .saturating_sub(self.arena.held())
                    .saturating_sub(self.register_bytes)
                    .saturating_sub(self.transient)
    }
}
