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
    #[cfg(test)]
    pub(super) fn substitute_identity(&mut self) -> bool {
        let Some((_, binding)) = self.identities.first_mut() else {
            return false;
        };
        *binding = binding.checked_add(1).expect("fixture binding");
        true
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
    // A statement action's Enter holds its atoms until its Commit.
    for (measure, coefficient) in crate::compile::room::charges::region_isa::terms(&event.delta) {
        if *coefficient > 0
            && let Measure::Local(owner @ Owner::Atom { .. }) = measure
            && let Some(slot) = resources::role(source, owner).and_then(Role::slot)
        {
            next.insert(slot);
        }
    }
    // A suspended exit is the frame control record's own dynamic root, so a
    // cleanup holds the same slots however it was entered; a restored exit
    // holds its value again only if it has one.
    if !matches!(draft.ops[pc], RegionOp::Call { .. })
        && !dynamic(&draft.ops[pc])
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

/// Whether `op`'s event output is a dynamic root of the frame's control
/// record rather than a slot: a suspended exit, or a restored one with no
/// value.
pub(super) fn dynamic(op: &RegionOp) -> bool {
    match op {
        RegionOp::Checked { kernel, .. } => match kernel.kind() {
            super::CheckedKind::Suspend { .. } => true,
            super::CheckedKind::Restore { exit, .. } | super::CheckedKind::Escape { exit, .. } => {
                !super::departures::valued(*exit)
            }
            _ => false,
        },
        _ => false,
    }
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
            // The machine carries an escaping exit out of the frame.
            super::CheckedKind::Escape { .. } => vec![],
            super::CheckedKind::Cleanup { at } => std::iter::once(*next)
                .chain(super::departures::restores_at(draft, at))
                .collect(),
            super::CheckedKind::Header {
                driver,
                ended,
                unchecked,
                ..
            } => std::iter::once(*next)
                .chain(*driver)
                .chain(*ended)
                .chain(*unchecked)
                .collect(),
            _ => vec![*next],
        },
        RegionOp::Branch { yes, no, .. } => vec![*yes, *no],
        // The machine resumes a delivering action at its Commit.
        RegionOp::Boundary { spec } => match spec.next() {
            Some(next) if spec.action().is_some() => vec![next],
            _ => vec![],
        },
        RegionOp::Effect { next, .. } => next.iter().copied().collect(),
        RegionOp::Return { .. } | RegionOp::Raise { .. } | RegionOp::Exit { .. } => vec![],
    }
}

/// Whether `op` reaches `successor` without taking its event: an exhausted
/// plain `for`, or a protocol header that dispatches to `next` or finds its
/// own cursor exhausted.
pub(super) fn unaccepted(op: &RegionOp, successor: super::Pc) -> bool {
    let RegionOp::Checked { kernel, .. } = op else {
        return false;
    };
    match kernel.kind() {
        super::CheckedKind::Cursor { exhausted, .. } => *exhausted == successor,
        super::CheckedKind::Header {
            stage,
            driver,
            ended,
            ..
        } => {
            stage.phase == super::LoopPhase::Step
                && (*driver == Some(successor) || *ended == Some(successor))
        }
        _ => false,
    }
}

/// What a raise at the event of row `base` holds, `live` standing before
/// it: a hook raises only once its call is entered (`K-ITER-005`), so what
/// the call moved into its callee is gone.
fn raised_at(
    draft: &RegionDraft,
    base: usize,
    mut live: BTreeSet<Slot>,
) -> Result<BTreeSet<Slot>, Refused> {
    if super::lower::hook_call(draft, base)? {
        for input in &draft.steps[base].event.inputs {
            if let Some(slot) = resources::role(&draft.source, input).and_then(Role::slot) {
                live.remove(&slot);
            }
        }
    }
    Ok(live)
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
    let events = draft.source.room.events.len();
    loop {
        while let Some(pc) = queue.pop_front() {
            let Some(live) = states[pc].clone() else {
                return Err(Refused::Exit);
            };
            let next = transfer(draft, pc, &live)?;
            for successor in successors(draft, pc) {
                let state = states.get_mut(successor.offset()).ok_or(Refused::Path)?;
                let next = if unaccepted(&draft.ops[pc], successor) {
                    &live
                } else {
                    &next
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
        // A raising event's own origin row is entered where the event's
        // base row stands, holding what it holds.
        for route in draft.departures.iter() {
            let origin = route.origin.offset();
            if origin < events || states.get(origin).ok_or(Refused::Path)?.is_some() {
                continue;
            }
            let at = &draft.steps[origin].at;
            let base = draft.steps[..events]
                .iter()
                .position(|step| step.at == *at)
                .ok_or(Refused::Path)?;
            if let Some(live) = states[base].clone() {
                states[origin] = Some(raised_at(draft, base, live)?);
                queue.push_back(origin);
            }
        }
        if queue.is_empty() {
            return Ok(states);
        }
    }
}
