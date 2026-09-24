// The heap's side of projection rebinding: finding the placeholders a root
// reaches, and replacing them inside the objects that hold them (FIG-3628).

use std::collections::BTreeSet;

use super::{Heap, ProjectedValue, Value};

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

    /// The unavailable projections `root` reaches, inline or through the heap,
    /// each once per object that holds it.
    pub(crate) fn unavailable_projections_from(&self, root: &Value) -> Vec<ProjectedValue> {
        let mut found = Vec::new();
        let mut visited = BTreeSet::new();
        let mut values = vec![root];
        while let Some(value) = values.pop() {
            match value {
                Value::Projected(projected) if projected.is_unavailable() => {
                    found.push(projected.clone());
                }
                Value::Tuple(items) | Value::List(items) => values.extend(items.iter()),
                Value::Record(record) => values.extend(record.values()),
                Value::Ref(id) => {
                    if visited.insert(*id)
                        && let Ok(object) = self.get(*id)
                    {
                        values.extend(object.values());
                    }
                }
                _ => {}
            }
        }
        found
    }
}
