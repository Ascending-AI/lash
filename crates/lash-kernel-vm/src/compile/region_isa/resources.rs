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
        roles: &[(Role, Reg<LeaseValue>)],
        amount: &Amount,
        output: Option<&Owner>,
    ) -> Result<Self, Refused> {
        let slots = source.owner_slots()?;
        // The region's roles are sorted by role, as `roles` builds them.
        let register = |role: Role| {
            roles
                .binary_search_by(|(candidate, _)| candidate.cmp(&role))
                .map(|index| roles[index].1)
                .map_err(|_| Refused::Facts)
        };
        let terms = terms(amount)
            .iter()
            .map(|(measure, coefficient)| {
                let input = match measure {
                    Measure::Local(owner) if Some(owner) == output => ResourceInput::Result,
                    Measure::Local(owner) => {
                        match slots.root(owner, doc::TaskId::MAIN, 0).and_then(root_role) {
                            Some(role) => ResourceInput::Local(register(role)?),
                            None => ResourceInput::SourceFact(measure.clone()),
                        }
                    }
                    Measure::Release(owners) => {
                        let mut released = std::collections::BTreeSet::new();
                        let mut external = false;
                        for owner in owners {
                            for root in owner_roots(source, owner)? {
                                match root_role(root) {
                                    Some(role) => {
                                        released.insert(role);
                                    }
                                    None => external = true,
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

pub(super) fn role(source: &Source, owner: &Owner) -> Option<Role> {
    let slots = source.owner_slots().ok()?;
    let root = slots.root(owner, doc::TaskId::MAIN, 0).or_else(|| {
        let Owner::Visible { at, name } = owner else {
            return None;
        };
        match crate::compile::room::root_owners::visible(&slots.scopes, at, name) {
            Some(binding) => slots.root(binding, doc::TaskId::MAIN, 0),
            // A name without a lexical binding is a session cell's.
            None => Some(Root::session(name)),
        }
    })?;
    root_role(root)
}

/// A name read at `at` holds a cell: a capture, or a binding a closure
/// shares in its own scope.
pub(super) fn cell(source: &Source, at: &Site, name: &lash_kernel_value::Name, role: Role) -> bool {
    role.capture()
        || source.owner_slots().is_ok_and(|slots| {
            matches!(
                crate::compile::room::root_owners::visible(&slots.scopes, at, name),
                Some(Owner::Binding { declared, name })
                    if source.shared.get(&(declared.clone(), name.clone())) == Some(&true)
            )
        })
}

/// The verified register role of an actual source root, if it has one.
pub(super) fn root_role(root: Root) -> Option<Role> {
    match root {
        Root::Value { slot, .. } => Some(Role::Slot(slot)),
        Root::Session(digest) => Some(Role::Session(digest)),
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
    match role(source, binding)? {
        Role::Slot(slot @ Slot::Binding(_)) => Some(slot),
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

pub(super) type RoleRegisters = Box<[(Role, Reg<LeaseValue>)]>;

pub(super) fn roles(source: &Source) -> Result<RoleRegisters, Refused> {
    // Many exits share their owners: each distinct owner expands once.
    let owners: std::collections::BTreeSet<&Owner> = source
        .room
        .exits
        .iter()
        .flat_map(|exit| exit.owners.iter())
        .collect();
    let mut roles = std::collections::BTreeSet::new();
    for owner in owners {
        for root in owner_roots(source, owner)? {
            roles.extend(root_role(root));
        }
    }
    roles
        .into_iter()
        .enumerate()
        .map(|(index, role)| Ok((role, Reg::new(index)?)))
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
