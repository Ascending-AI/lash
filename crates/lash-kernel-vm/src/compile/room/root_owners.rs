//! Canonical owner identities and lexical resolution, cached with the source.
//! Registers and frame-cache positions do not participate in these identities.
use super::*;
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::{Owner as Root, Slot};
use std::collections::BTreeSet;

type Scopes = BTreeMap<Owner, Site>;
type Aliases = BTreeMap<Owner, Owner>;
type Resolution = (Scopes, Aliases, BTreeMap<Owner, (u32, bool)>);
pub(super) fn form(source: &Source, owners: &BTreeSet<Owner>) -> Result<Resolution, Refused> {
    let binding_slots: BTreeMap<_, _> = source
        .bindings
        .iter()
        .map(|binding| {
            (
                Owner::Binding {
                    declared: binding.declared.clone(),
                    name: binding.name.clone(),
                },
                (binding.index, binding.borrowed),
            )
        })
        .collect();
    let (scopes, aliases) = resolve(source, owners)?;
    Ok((scopes, aliases, binding_slots))
}
fn resolve(source: &Description, owners: &BTreeSet<Owner>) -> Result<(Scopes, Aliases), Refused> {
    let mut scopes = BTreeMap::new();
    for owner in owners {
        if let Owner::Binding { declared, .. } = owner {
            let mut node = doc::Node::Block(&source.body);
            if declared.unit != source.site.unit || !declared.path.starts_with(&source.site.path) {
                return Err(Refused::Source);
            }
            for index in &declared.path[source.site.path.len()..] {
                node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
            }
            let scope = match node {
                doc::Node::Block(_) => declared.clone(),
                doc::Node::Expr(doc::Expr::Let { .. }) => child(declared, 1),
                doc::Node::Stmt(doc::Stmt::Let { .. }) => {
                    let mut scope = declared.clone();
                    scope.path.pop().ok_or(Refused::Path)?;
                    scope
                }
                _ => return Err(Refused::Source),
            };
            scopes.insert(owner.clone(), scope);
        }
    }
    let mut aliases = BTreeMap::new();
    for owner in owners {
        if let Owner::Selected(at) = owner {
            aliases.insert(owner.clone(), Owner::Operand(at.clone()));
        }
        if let Owner::Visible { at, name } = owner {
            let selected = scopes
                .iter()
                .filter(|(candidate, scope)| {
                    let Owner::Binding {
                        declared,
                        name: candidate_name,
                    } = candidate
                    else {
                        return false;
                    };
                    if candidate_name != name
                        || at.unit != scope.unit
                        || !at.path.starts_with(&scope.path)
                    {
                        return false;
                    }
                    if declared.path.len() == scope.path.len() + 1
                        && declared.path.starts_with(&scope.path)
                    {
                        return at.path.get(scope.path.len()) > declared.path.last();
                    }
                    true
                })
                .max_by_key(|(candidate, scope)| (scope.path.len(), (*candidate).clone()));
            if let Some((binding, _)) = selected {
                aliases.insert(owner.clone(), binding.clone());
            } else if source.site.unit == Unit::Main {
                aliases.insert(
                    owner.clone(),
                    Owner::SessionCell {
                        at: source.site.clone(),
                        name: name.clone(),
                    },
                );
            }
        }
    }
    Ok((scopes, aliases))
}
pub(super) fn cell_assignments(source: &Description) -> Result<BTreeMap<Site, Name>, Refused> {
    let mut owners = BTreeSet::new();
    for (declared, name) in source.shared.keys() {
        owners.insert(Owner::Binding {
            declared: declared.clone(),
            name: name.clone(),
        });
    }
    for name in &source.params {
        owners.insert(Owner::Binding {
            declared: source.site.clone(),
            name: name.clone(),
        });
    }
    let mut pending = vec![(source.site.clone(), doc::Node::Block(&source.body))];
    while let Some((at, node)) = pending.pop() {
        match node {
            doc::Node::Stmt(doc::Stmt::Let { name, .. })
            | doc::Node::Expr(doc::Expr::Let { name, .. }) => {
                owners.insert(Owner::Binding {
                    declared: at.clone(),
                    name: name.clone(),
                });
            }
            doc::Node::Stmt(doc::Stmt::For { binding, .. }) => {
                owners.insert(Owner::Binding {
                    declared: child(&at, 1),
                    name: binding.clone(),
                });
            }
            doc::Node::Stmt(doc::Stmt::Try(doc::TryStmt {
                catch: Some(catch), ..
            })) => {
                owners.insert(Owner::Binding {
                    declared: child(&at, 1),
                    name: catch.binding.clone(),
                });
            }
            doc::Node::Stmt(doc::Stmt::Assign {
                place: doc::Place::Variable(name),
                ..
            }) => {
                owners.insert(Owner::Visible {
                    at: at.clone(),
                    name: name.clone(),
                });
            }
            _ => {}
        }
        pending.extend(
            node.children()
                .into_iter()
                .enumerate()
                .map(|(index, node)| (child(&at, index), node)),
        );
    }
    let (_, aliases) = resolve(source, &owners)?;
    Ok(aliases
        .into_iter()
        .filter_map(|(owner, binding)| match (owner, binding) {
            (Owner::Visible { at, name }, Owner::Binding { declared, .. })
                if source.shared.get(&(declared.clone(), name.clone())) == Some(&true)
                    && source.persistent.get(&declared) != Some(&name) =>
            {
                Some((at, name))
            }
            _ => None,
        })
        .collect())
}
impl OwnerSlots {
    pub(crate) fn continuation(&self, site: &Site, task: TaskId, invocation: u64) -> Option<Root> {
        let Root::Value {
            slot: Slot::Operand(index),
            ..
        } = self.root(&Owner::Operand(site.clone()), task, invocation)?
        else {
            return None;
        };
        Some(Root::Value {
            task,
            invocation,
            slot: Slot::Continuation(index),
        })
    }
    pub(crate) fn control_roots(
        &self,
        task: TaskId,
        invocation: u64,
    ) -> impl Iterator<Item = Root> + '_ {
        self.controls
            .iter()
            .filter_map(move |owner| self.root(owner, task, invocation))
    }
    pub(crate) fn root(&self, owner: &Owner, task: TaskId, invocation: u64) -> Option<Root> {
        let owner = self.aliases.get(owner).unwrap_or(owner);
        if let Owner::SessionCell { name, .. } = owner {
            return Some(Root::session(name));
        }
        if let Owner::Frame(at) = owner {
            return (at == &self.source).then_some(Root::Frame { task, invocation });
        }
        let index = self
            .binding_slots
            .get(owner)
            .map(|(index, _)| *index)
            .or_else(|| self.get(owner))?;
        let slot = match owner {
            Owner::Operand(_) | Owner::Selected(_) | Owner::Atom { .. } => Slot::Operand(index),
            Owner::Binding { .. }
                if self
                    .binding_slots
                    .get(owner)
                    .is_some_and(|(_, borrowed)| *borrowed) =>
            {
                Slot::Capture(index)
            }
            Owner::Binding { .. } => Slot::Binding(index),
            Owner::HookArgument { .. } => Slot::Argument(index),
            Owner::HookResult { .. } => Slot::Control(index),
            Owner::LoopCursor(_) | Owner::LoopHeader(_) => Slot::Control(index),
            Owner::Departure(_) | Owner::PendingDeparture(_) | Owner::ActiveDeparture(_) => {
                Slot::Departure(index)
            }
            _ => return None,
        };
        Some(Root::Value {
            task,
            invocation,
            slot,
        })
    }
    pub(crate) fn group(
        &self,
        group: &Owner,
        task: TaskId,
        invocation: u64,
    ) -> Option<Box<[Root]>> {
        let included = |owner: &Owner| match (group, owner) {
            (Owner::Scope(at), Owner::Binding { .. }) => self
                .scopes
                .get(owner)
                .is_some_and(|scope| at.unit == scope.unit && scope.path.starts_with(&at.path)),
            (Owner::Operands(at), Owner::Operand(site) | Owner::Selected(site))
            | (
                Owner::Operands(at),
                Owner::Atom { at: site, .. }
                | Owner::HookArgument { at: site, .. }
                | Owner::HookResult { at: site, .. }
                | Owner::LoopCursor(site)
                | Owner::LoopHeader(site),
            ) => at.unit == site.unit && site.path.starts_with(&at.path),
            (
                Owner::Departures(at),
                Owner::Departure(site)
                | Owner::PendingDeparture(site)
                | Owner::ActiveDeparture(site),
            ) => at.unit == site.unit && site.path.starts_with(&at.path),
            (Owner::Sessions(_), Owner::SessionCell { .. }) => true,
            _ => false,
        };
        if !matches!(
            group,
            Owner::Scope(_) | Owner::Operands(_) | Owner::Departures(_) | Owner::Sessions(_)
        ) {
            return None;
        }
        Some(
            self.slots
                .keys()
                .filter(|owner| included(owner))
                .filter_map(|owner| self.root(owner, task, invocation))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect(),
        )
    }
}
