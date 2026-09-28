// The heap's side of projection rebinding: replacing the placeholders a
// reload left inside the objects that hold them (FIG-3628).

use super::Heap;

impl Heap {
    /// Rebinds the unavailable projections heap objects hold, in place, so
    /// every holder of an object sees the rebound value.
    pub(crate) fn rebind_projections(
        &mut self,
        rebind: &mut crate::runtime::projected_refresh::Rebind<'_>,
    ) {
        let mut changed = Vec::new();
        for (id, entry) in &mut self.entries {
            if crate::runtime::projected_refresh::rebind_object(&mut entry.object, rebind) {
                changed.push(*id);
            }
        }
        for id in changed {
            self.invalidate_materialized_reaching(id);
        }
    }
}
