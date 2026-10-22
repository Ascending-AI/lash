//! Admission acquired before graph copies and decoder materialization.
use std::collections::BTreeMap;

use lash_protocol_types::allocation::{
    AllocationBudget, AllocationError, AllocationHold, AllocationReservation,
};

use super::{BlobEncoding, DecodeLimits, EncodedNode, GraphRefusal, NodeId, NodeSource};

/// Builder metadata and every growing buffer are admitted before allocation.
/// Its final obligation can be transferred into the freshly decoded source.
pub(crate) struct DecodeAdmission {
    budget: AllocationBudget,
    holds: Vec<AllocationHold>,
}
struct Holds(Vec<AllocationHold>);
impl AllocationReservation for Holds {
    fn retain(&self) {
        for hold in &self.0 {
            hold.clone().into_retained();
        }
    }
}
impl DecodeAdmission {
    pub(crate) fn unowned() -> Self {
        Self {
            budget: AllocationBudget::unowned(),
            holds: Vec::new(),
        }
    }
    pub(crate) fn new_in(budget: &AllocationBudget) -> Result<Self, AllocationError> {
        let holds = if budget.identity() == 0 {
            Vec::new()
        } else {
            let metadata = budget.reserve(512)?;
            vec![metadata]
        };
        Ok(Self {
            budget: budget.clone(),
            holds,
        })
    }
    pub(crate) fn budget(&self) -> &AllocationBudget {
        &self.budget
    }
    pub(crate) fn reserve(&mut self, bytes: u64) -> Result<(), AllocationError> {
        if self.budget.identity() == 0 {
            return Ok(());
        }
        let capacity = if self.holds.len() == self.holds.capacity() {
            self.holds.capacity().saturating_mul(2).max(4)
        } else {
            0
        };
        let hold = self.budget.reserve(
            bytes.saturating_add(
                (capacity as u64)
                    .saturating_mul(2 * std::mem::size_of::<AllocationHold>() as u64)
                    .saturating_add(128),
            ),
        )?;
        if capacity != 0 {
            self.holds.reserve_exact(capacity - self.holds.len());
        }
        self.holds.push(hold);
        Ok(())
    }
    pub(crate) fn finish(self) -> AllocationHold {
        if self.holds.is_empty() {
            AllocationHold::default()
        } else {
            AllocationHold::new(Holds(self.holds))
        }
    }
}

pub(super) fn closure_parts(
    source: &dyn NodeSource,
    root: NodeId,
    limits: &DecodeLimits,
    budget: &AllocationBudget,
) -> Result<(BTreeMap<NodeId, EncodedNode>, AllocationHold), GraphRefusal> {
    let mut admission = DecodeAdmission::new_in(budget)?;
    admission.reserve(1024)?;
    let mut nodes = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if nodes.contains_key(&id) {
            continue;
        }
        let stored = source.fetch(&id).ok_or(GraphRefusal::MissingNode(id))?;
        if stored.encoding != BlobEncoding::Graph {
            continue;
        }
        // Checking uses token/envelope scratch before the map or pending frontier grows.
        admission.reserve(
            (stored.bytes.len() as u64)
                .saturating_mul(64)
                .saturating_add(2048),
        )?;
        let envelope = super::node::check_envelope(stored.bytes, id, limits)?;
        admission.reserve(
            (envelope.children.len() as u64)
                .saturating_mul(128)
                .saturating_add(2048),
        )?;
        if !super::node::is_semantic(envelope.kind) {
            pending.extend(envelope.children.iter());
        }
        nodes.insert(
            id,
            EncodedNode {
                id,
                bytes: super::SharedBytes::copy_from_slice_in(stored.bytes, budget)?,
            },
        );
    }
    Ok((nodes, admission.finish()))
}

/// Check and copy a borrowed graph under its owner before exposing readers.
/// Byte carriers have independent obligations; the map owns its metadata hold.
pub fn closure_in(
    source: &dyn NodeSource,
    root: NodeId,
    limits: &DecodeLimits,
    budget: &AllocationBudget,
) -> Result<crate::source_map::SharedSourceMap<NodeId, EncodedNode>, GraphRefusal> {
    let (nodes, allocation) = closure_parts(source, root, limits, budget)?;
    Ok(crate::source_map::SharedSourceMap::from_admitted(
        nodes,
        allocation,
        budget.clone(),
    ))
}
