//! Bounded cold reconstruction tables; no history of loop occurrences.
use super::Reg;
use crate::compile::room::Point;
use lash_kernel_state::kernel_internals::logical::LeaseValue;
use lash_kernel_state::kernel_internals::logical::Slot;

/// A verified root role: a frame slot, or a durable main session cell. A
/// session role is a ledger root of its own and never a persisted frame slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Role {
    Slot(Slot),
    Session([u8; 32]),
}
impl Role {
    pub(crate) fn slot(self) -> Option<Slot> {
        match self {
            Self::Slot(slot) => Some(slot),
            Self::Session(_) => None,
        }
    }
    pub(crate) fn capture(self) -> bool {
        matches!(self, Self::Slot(Slot::Capture(_)))
    }
}

#[derive(Clone)]
pub(crate) struct RecoveryMap {
    pub(crate) roles: Box<[(Slot, Reg<LeaseValue>)]>,
    pub(crate) operands: Box<[Reg<LeaseValue>]>,
    pub(crate) pending: Option<Reg<LeaseValue>>,
    pub(crate) continuation: Point,
    pub(crate) graph: super::GraphRecovery,
}

/// Untrusted inside RegionDraft; verified only as part of immutable RegionCode.
/// These tables grant no state or semantic authority on their own.
#[derive(Clone)]
pub(crate) struct GraphRecovery {
    source: Point,
    roles: Box<[(Slot, Reg<LeaseValue>)]>,
    pending: Option<Reg<LeaseValue>>,
    incoming: Option<Reg<LeaseValue>>,
    scratch: Box<[Reg<LeaseValue>]>,
    release_phase: u32,
    versions: Box<[(Reg<LeaseValue>, u32)]>,
    identities: Box<[(Reg<LeaseValue>, u32)]>,
}
impl GraphRecovery {
    pub(super) fn from_roles(
        source: &Point,
        roles: &[(Slot, Reg<LeaseValue>)],
        pending: Option<Reg<LeaseValue>>,
        phase: u32,
        incoming: Option<Reg<LeaseValue>>,
    ) -> Self {
        Self {
            source: source.clone(),
            roles: roles.into(),
            pending,
            incoming,
            scratch: pending.into_iter().collect(),
            release_phase: phase,
            versions: roles
                .iter()
                .map(|(_, reg)| (*reg, reg.index()))
                .chain(pending.map(|reg| (reg, reg.index())))
                .collect(),
            identities: roles
                .iter()
                .map(|(_, reg)| (*reg, reg.index()))
                .chain(pending.map(|reg| (reg, reg.index())))
                .collect(),
        }
    }
    pub(super) fn check(
        &self,
        source: &Point,
        roles: &[(Slot, Reg<LeaseValue>)],
        pending: Option<Reg<LeaseValue>>,
        phase: u32,
        incoming: Option<Reg<LeaseValue>>,
    ) -> bool {
        self.source == *source
            && self.roles.as_ref() == roles
            && self.pending == pending
            && self.incoming == incoming
            && self.release_phase == phase
            && self.scratch.iter().copied().eq(pending)
            && self.versions.iter().copied().eq(roles
                .iter()
                .map(|(_, reg)| (*reg, reg.index()))
                .chain(pending.map(|reg| (reg, reg.index()))))
            && self.identities.iter().copied().eq(roles
                .iter()
                .map(|(_, reg)| (*reg, reg.index()))
                .chain(pending.map(|reg| (reg, reg.index()))))
    }

    /// This pending register is the canonical task Incoming holding, rather
    /// than an extra frame Slot or an unaccepted computed result.
    pub(crate) fn incoming(&self) -> Option<Reg<LeaseValue>> {
        self.incoming
    }
    pub(crate) fn roles(&self) -> &[(Slot, Reg<LeaseValue>)] {
        &self.roles
    }
    pub(crate) fn scratch(&self) -> &[Reg<LeaseValue>] {
        &self.scratch
    }
    /// Binding-slot indices, resolved to actual current object versions by C/D.
    pub(crate) fn versions(&self) -> &[(Reg<LeaseValue>, u32)] {
        &self.versions
    }
    /// Binding-slot indices, never predicted or reused source object identities.
    pub(crate) fn identities(&self) -> &[(Reg<LeaseValue>, u32)] {
        &self.identities
    }
    pub(crate) fn release_phase(&self) -> u32 {
        self.release_phase
    }
    #[cfg(test)]
    pub(super) fn substitute_identity(&mut self) -> bool {
        let Some((_, binding)) = self.identities.first_mut() else {
            return false;
        };
        *binding = binding.checked_add(1).expect("fixture binding");
        true
    }
    #[cfg(any(test, feature = "testing"))]
    pub(super) fn fixture(source: &Point) -> Self {
        Self {
            source: source.clone(),
            roles: Box::new([]),
            pending: None,
            incoming: None,
            scratch: Box::new([]),
            release_phase: 0,
            versions: Box::new([]),
            identities: Box::new([]),
        }
    }
}

use super::{Refused, RegionDraft, RegionOp, resources};
use crate::compile::room::{Measure, Owner, Phase};
use lash_kernel_doc as doc;
use std::collections::BTreeSet;

/// Static frame-slot liveness. A session role is a durable ledger root whose
/// holding is dynamic (an earlier run may have bound it), so it never enters
/// a recovery map; the machine tracks and publishes it.
pub(super) fn transfer(
    draft: &RegionDraft,
    pc: usize,
    live: &BTreeSet<Slot>,
) -> Result<BTreeSet<Slot>, Refused> {
    let step = draft.steps.get(pc).ok_or(Refused::Path)?;
    if step.stage.is_some() {
        return Ok(live.clone());
    }
    let event = &step.event;
    let source = &draft.source;
    let mut next = live.clone();
    for input in &event.inputs {
        if let Some(slot) = resources::role(source, input).and_then(Role::slot) {
            next.remove(&slot);
        }
    }
    for (measure, coefficient) in crate::compile::room::charges::region_isa::terms(&event.delta) {
        if *coefficient < 0
            && let Measure::Release(owners) = measure
        {
            for owner in owners {
                for root in resources::owner_roots(source, owner)? {
                    if let Some(slot) = resources::root_role(root).and_then(Role::slot) {
                        next.remove(&slot);
                    }
                }
            }
        }
    }
    if !matches!(draft.ops[pc], RegionOp::Call { .. })
        && let Some(output) = event
            .output
            .as_ref()
            .and_then(|owner| resources::role(source, owner))
            .and_then(Role::slot)
    {
        next.insert(output);
    }
    if event.point.phase == Phase::Commit
        && let doc::Node::Stmt(doc::Stmt::Assign {
            place: doc::Place::Variable(name),
            ..
        }) = resources::node(source, &event.point.site)?
        && let Some(slot) = resources::role(
            source,
            &Owner::Visible {
                at: event.point.site.clone(),
                name: name.clone(),
            },
        )
        .and_then(Role::slot)
    {
        next.insert(slot);
    }
    Ok(next)
}

pub(super) fn successors(draft: &RegionDraft, pc: usize) -> Vec<super::Pc> {
    let op = &draft.ops[pc];
    if matches!(op, RegionOp::Return { .. }) {
        return super::departures::successor(draft, super::Pc(pc as u32))
            .into_iter()
            .collect();
    }
    match op {
        RegionOp::Constant { next, .. }
        | RegionOp::Move { next, .. }
        | RegionOp::IntBinary { next, .. }
        | RegionOp::Read { next, .. }
        | RegionOp::Heap { next, .. }
        | RegionOp::Call { next, .. } => vec![*next],
        RegionOp::Checked { kernel, next, .. } => match kernel.kind() {
            super::CheckedKind::Cursor { exhausted, .. } => vec![*next, *exhausted],
            _ => vec![*next],
        },
        RegionOp::Branch { yes, no, .. } => vec![*yes, *no],
        RegionOp::Boundary { .. } => vec![],
        RegionOp::Return { .. } | RegionOp::Exit { .. } => vec![],
    }
}

pub(super) fn liveness(draft: &RegionDraft) -> Result<Vec<Option<BTreeSet<Slot>>>, Refused> {
    let mut states = vec![None; draft.ops.len()];
    let mut initial = BTreeSet::new();
    for name in &draft.source.params {
        if let Some(slot) = resources::role(
            &draft.source,
            &Owner::Binding {
                declared: draft.source.site.clone(),
                name: name.clone(),
            },
        )
        .and_then(Role::slot)
        {
            initial.insert(slot);
        }
    }
    initial.extend(
        draft
            .roles
            .iter()
            .filter_map(|(role, _)| role.slot().filter(|slot| matches!(slot, Slot::Capture(_)))),
    );
    states
        .get_mut(draft.entry.offset())
        .ok_or(Refused::Path)?
        .replace(initial);
    let mut queue = std::collections::VecDeque::from([draft.entry.offset()]);
    while let Some(pc) = queue.pop_front() {
        let Some(live) = states[pc].clone() else {
            return Err(Refused::Exit);
        };
        let next = transfer(draft, pc, &live)?;
        for successor in successors(draft, pc) {
            let state = states.get_mut(successor.offset()).ok_or(Refused::Path)?;
            let next = match &draft.ops[pc] {
                RegionOp::Checked { kernel, .. } if matches!(kernel.kind(), super::CheckedKind::Cursor { exhausted, .. } if *exhausted == successor) => {
                    &live
                }
                _ => &next,
            };
            match state {
                None => {
                    *state = Some(next.clone());
                    queue.push_back(successor.offset());
                }
                Some(existing) if existing != next => {
                    return Err(Refused::Exit);
                }
                Some(_) => {}
            }
        }
    }
    Ok(states)
}
