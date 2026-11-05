//! Indexed exact source amounts. Stable operations need no owner map lookup.
use super::*;
use crate::compile::room::charges::region_isa::terms;
use crate::compile::room::{Amount, Measure, Owner};
use lash_kernel_state::kernel_internals::logical::Owner as Root;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResourceInput {
    Local(Reg<LeaseValue>),
    Result,
    Release(Box<[Reg<LeaseValue>]>),
    /// Actual source-qualified graph, control or call facts from the lease.
    SourceFact(Measure),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexedAmount {
    pub(crate) fixed: i128,
    pub(crate) terms: Box<[(ResourceInput, i128)]>,
}
impl IndexedAmount {
    pub(crate) fn evaluate(
        &self,
        mut fact: impl FnMut(&ResourceInput) -> Option<i128>,
    ) -> Option<i128> {
        self.terms
            .iter()
            .try_fold(self.fixed, |sum, (input, coefficient)| {
                sum.checked_add(fact(input)?.checked_mul(*coefficient)?)
            })
    }
    pub(super) fn compile(
        source: &Source,
        roles: &[(Slot, Reg<LeaseValue>)],
        amount: &Amount,
        output: Option<&Owner>,
    ) -> Result<Self, Refused> {
        let slots = source.owner_slots()?;
        let register = |slot| {
            roles
                .iter()
                .find(|(candidate, _)| *candidate == slot)
                .map(|(_, reg)| *reg)
                .ok_or(Refused::Facts)
        };
        let terms = terms(amount)
            .iter()
            .map(|(measure, coefficient)| {
                let input = match measure {
                    Measure::Local(owner) if Some(owner) == output => ResourceInput::Result,
                    Measure::Local(owner) => match slots.root(owner, doc::TaskId::MAIN, 0) {
                        Some(Root::Value { slot, .. }) => ResourceInput::Local(register(slot)?),
                        _ => ResourceInput::SourceFact(measure.clone()),
                    },
                    Measure::Release(owners) => {
                        let mut released = std::collections::BTreeSet::new();
                        let mut external = false;
                        for owner in owners {
                            for root in owner_roots(source, owner)? {
                                match root {
                                    Root::Value { slot, .. } => {
                                        released.insert(slot);
                                    }
                                    _ => external = true,
                                }
                            }
                        }
                        if external {
                            ResourceInput::SourceFact(measure.clone())
                        } else {
                            ResourceInput::Release(
                                released
                                    .into_iter()
                                    .map(register)
                                    .collect::<Result<_, _>>()?,
                            )
                        }
                    }
                    _ => ResourceInput::SourceFact(measure.clone()),
                };
                Ok((input, *coefficient))
            })
            .collect::<Result<_, Refused>>()?;
        Ok(Self {
            fixed: amount.fixed,
            terms,
        })
    }
}

pub(super) fn slot(source: &Source, owner: &Owner) -> Option<Slot> {
    let slots = source.owner_slots().ok()?;
    let root = slots.root(owner, doc::TaskId::MAIN, 0).or_else(|| {
        let Owner::Visible { at, name } = owner else {
            return None;
        };
        let binding = crate::compile::room::root_owners::visible(&slots.scopes, at, name)?;
        slots.root(binding, doc::TaskId::MAIN, 0)
    })?;
    match root {
        Root::Value { slot, .. } => Some(slot),
        _ => None,
    }
}

/// Only an unshared lexical binding retains the same base across operand calls.
pub(super) fn retained_read(source: &Source, at: &Site) -> Option<Slot> {
    let doc::Node::Expr(doc::Expr::Variable(name)) = node(source, at).ok()? else {
        return None;
    };
    let slots = source.owner_slots().ok()?;
    let binding = crate::compile::room::root_owners::visible(&slots.scopes, at, name)?;
    let Owner::Binding { declared, name } = binding else {
        return None;
    };
    if source.shared.get(&(declared.clone(), name.clone())) == Some(&true) {
        return None;
    }
    match slot(source, binding)? {
        slot @ Slot::Binding(_) => Some(slot),
        _ => None,
    }
}

pub(super) fn node<'a>(source: &'a Source, at: &Site) -> Result<doc::Node<'a>, Refused> {
    if at.unit != source.site.unit || !at.path.starts_with(&source.site.path) {
        return Err(Refused::Path);
    }
    let mut node = doc::Node::Block(source.body.as_ref());
    for index in &at.path[source.site.path.len()..] {
        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
    }
    Ok(node)
}

pub(super) type RoleRegisters = Box<[(Slot, Reg<LeaseValue>)]>;

pub(super) fn roles(source: &Source) -> Result<RoleRegisters, Refused> {
    let mut roles = std::collections::BTreeSet::new();
    for exit in &source.room.exits {
        for owner in &exit.owners {
            for root in owner_roots(source, owner)? {
                if let Root::Value { slot, .. } = root {
                    roles.insert(slot);
                }
            }
        }
    }
    roles
        .into_iter()
        .enumerate()
        .map(|(index, slot)| Ok((slot, Reg::new(index)?)))
        .collect()
}

pub(super) fn owner_roots(source: &Source, owner: &Owner) -> Result<Box<[Root]>, Refused> {
    let slots = source.owner_slots()?;
    if let Some(root) = slots.root(owner, doc::TaskId::MAIN, 0) {
        return Ok(Box::new([root]));
    }
    slots
        .group(owner, doc::TaskId::MAIN, 0)
        .ok_or(Refused::Source)
}
