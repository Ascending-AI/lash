//! Canonical owner identities and lexical resolution, cached with the source.
//! Registers and frame-cache positions do not participate in these identities.
use super::*;
use lash_kernel_doc::TaskId;
use lash_kernel_state::kernel_internals::logical::{Owner as Root, RoleId, Slot, SourceRole};
use std::collections::BTreeSet;

type Scopes = BTreeMap<Owner, Site>;
type Aliases = BTreeMap<Owner, Owner>;
type Resolution = (Scopes, Aliases, BTreeMap<Owner, (RoleId, bool)>);

/// The persisted discriminator of an owner a frame slot can name.
pub(super) fn source_role(owner: &Owner) -> Option<SourceRole> {
    Some(match owner {
        Owner::Operand(at) => SourceRole::Operand(at.clone()),
        Owner::Selected(at) => SourceRole::Selected(at.clone()),
        Owner::Atom { at, index } => SourceRole::Atom {
            at: at.clone(),
            index: *index,
        },
        Owner::LoopCursor(at) => SourceRole::LoopCursor(at.clone()),
        Owner::LoopHeader(at) => SourceRole::LoopHeader(at.clone()),
        Owner::PendingDeparture(at) => SourceRole::PendingDeparture(at.clone()),
        Owner::ActiveDeparture(at) => SourceRole::ActiveDeparture(at.clone()),
        Owner::Binding { declared, name } => SourceRole::Binding {
            declared: declared.clone(),
            name: name.clone(),
        },
        _ => return None,
    })
}
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
                (
                    SourceRole::Binding {
                        declared: binding.declared.clone(),
                        name: binding.name.clone(),
                    }
                    .id(),
                    binding.borrowed,
                ),
            )
        })
        .collect();
    let (scopes, mut aliases) = resolve(source, owners)?;
    // A name the body only reads names a session cell too: the session
    // group holds every cell the body names, read or written.
    let mut pending = vec![(source.site.clone(), doc::Node::Block(&source.body))];
    while let Some((at, node)) = pending.pop() {
        let read = match node {
            doc::Node::Expr(doc::Expr::Variable(name)) => Some(name),
            doc::Node::Expr(doc::Expr::Act(act)) => match &act.kind {
                doc::ActKind::Call(doc::Callee::Value(name))
                | doc::ActKind::Spawn(doc::Callee::Value(name)) => Some(name),
                _ => None,
            },
            _ => None,
        };
        let atoms = match node {
            doc::Node::Action(action) => action_atoms(action),
            _ => &[],
        };
        for name in read.into_iter().chain(atoms.iter().filter_map(|atom| {
            if let doc::Atom::Variable(name) = atom {
                Some(name)
            } else {
                None
            }
        })) {
            if visible(&scopes, &at, name).is_none() {
                aliases
                    .entry(Owner::Visible {
                        at: at.clone(),
                        name: name.clone(),
                    })
                    .or_insert_with(|| Owner::SessionCell {
                        at: source.site.clone(),
                        name: name.clone(),
                    });
            }
        }
        pending.extend(
            node.children()
                .into_iter()
                .enumerate()
                .map(|(index, node)| (child(&at, index), node)),
        );
    }
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
            // A name no binding of the unit declares is the session's: main
            // reads its own cells, and a closure resolves its free names
            // there once its lexical bindings and captures are excluded
            // (`Scopes::resolve_in`); a capture is a binding at the body.
            if let Some(binding) = visible(&scopes, at, name) {
                aliases.insert(owner.clone(), binding.clone());
            } else {
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
    // A resident for retains its iterable as its source cursor across body
    // statements. It is not an ephemeral expression operand after ForIn.
    for owner in owners {
        let Owner::LoopCursor(at) = owner else {
            continue;
        };
        let mut node = doc::Node::Block(&source.body);
        for index in &at.path[source.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        if matches!(node, doc::Node::Stmt(doc::Stmt::For { via: None, .. })) {
            aliases.insert(Owner::Operand(child(at, 0)), owner.clone());
        }
    }
    Ok((scopes, aliases))
}

pub(crate) fn visible<'a>(scopes: &'a Scopes, at: &Site, name: &Name) -> Option<&'a Owner> {
    scopes
        .iter()
        .filter(|(candidate, scope)| {
            let Owner::Binding {
                declared,
                name: candidate_name,
            } = candidate
            else {
                return false;
            };
            if candidate_name != name || at.unit != scope.unit || !at.path.starts_with(&scope.path)
            {
                return false;
            }
            if declared.path.len() == scope.path.len() + 1 && declared.path.starts_with(&scope.path)
            {
                return at.path.get(scope.path.len()) > declared.path.last();
            }
            true
        })
        .max_by_key(|(candidate, scope)| (scope.path.len(), (*candidate).clone()))
        .map(|(binding, _)| binding)
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
    /// The slot a resolved (non-alias) owner names in a frame of this body.
    fn slot(&self, owner: &Owner) -> Option<Slot> {
        let (id, borrowed) = match self.binding_slots.get(owner) {
            Some(&(id, borrowed)) => (id, borrowed),
            None => (self.get(owner)?, false),
        };
        Some(match owner {
            Owner::Operand(_) | Owner::Selected(_) | Owner::Atom { .. } => Slot::Operand(id),
            Owner::Binding { .. } if borrowed => Slot::Capture(id),
            Owner::Binding { .. } => Slot::Binding(id),
            Owner::LoopCursor(_) | Owner::LoopHeader(_) => Slot::Control(id),
            Owner::PendingDeparture(_) | Owner::ActiveDeparture(_) => Slot::Departure(id),
            _ => return None,
        })
    }
    /// Every slot this body names with its source role; two roles under one
    /// slot refuse the body (Q4: the table is injective).
    pub(super) fn role_table(&self) -> Result<BTreeMap<Slot, SourceRole>, Refused> {
        let mut roles = BTreeMap::new();
        let mut insert = |slot: Slot, role: SourceRole| match roles.insert(slot, role.clone()) {
            Some(other) if other != role => Err(Refused::Facts),
            _ => Ok(()),
        };
        for owner in self.slots.keys().chain(self.binding_slots.keys()) {
            let (Some(slot), Some(role)) = (self.slot(owner), source_role(owner)) else {
                continue;
            };
            if let Owner::Operand(_) = owner {
                insert(Slot::Continuation(slot.id()), role.clone())?;
            }
            insert(slot, role)?;
        }
        Ok(roles)
    }
    /// The source name of the durable main session cell with this owner digest.
    pub(crate) fn session_name(&self, digest: &[u8; 32]) -> Option<&Name> {
        self.slots
            .keys()
            .chain(self.aliases.values())
            .find_map(|owner| match owner {
                Owner::SessionCell { name, .. }
                    if Root::session(name) == Root::Session(*digest) =>
                {
                    Some(name)
                }
                _ => None,
            })
    }
    pub(crate) fn continuation(&self, site: &Site, task: TaskId, invocation: u64) -> Option<Root> {
        let id = self.get(&Owner::Operand(site.clone()))?;
        Some(Root::Value {
            task,
            invocation,
            slot: Slot::Continuation(id),
        })
    }
    pub(crate) fn root(&self, owner: &Owner, task: TaskId, invocation: u64) -> Option<Root> {
        let mut owner = owner;
        while let Some(target) = self.aliases.get(owner) {
            owner = target;
        }
        if let Owner::SessionCell { name, .. } = owner {
            return Some(Root::session(name));
        }
        if matches!(owner, Owner::Incoming(_)) {
            return self.contains(owner).then_some(Root::Incoming(task));
        }
        if let Owner::Frame(at) = owner {
            return (at == &self.source).then_some(Root::Frame { task, invocation });
        }
        Some(Root::Value {
            task,
            invocation,
            slot: self.slot(owner)?,
        })
    }
    /// A group's members are fixed by this immutable body; only the
    /// invocation stamp varies, so each group is resolved once.
    pub(crate) fn group(
        &self,
        group: &Owner,
        task: TaskId,
        invocation: u64,
    ) -> Option<Box<[Root]>> {
        let stamp = |root: &Root| match *root {
            Root::Value { slot, .. } => Root::Value {
                task,
                invocation,
                slot,
            },
            other => other,
        };
        self.with_group(group, |members| members.iter().map(stamp).collect())
    }
    /// Members under one placeholder invocation. Every member is either a
    /// value slot of that invocation or a session root, so the order of the
    /// set does not depend on the invocation it is later stamped with.
    fn resolve_group(&self, group: &Owner) -> Box<[Root]> {
        let included = |owner: &Owner| match (group, owner) {
            (Owner::Scope(at), Owner::Binding { .. }) => self
                .scopes
                .get(owner)
                .is_some_and(|scope| at.unit == scope.unit && scope.path.starts_with(&at.path)),
            (Owner::Operands(at), Owner::Operand(site) | Owner::Selected(site))
            | (
                Owner::Operands(at),
                Owner::Atom { at: site, .. } | Owner::LoopCursor(site) | Owner::LoopHeader(site),
            ) => at.unit == site.unit && site.path.starts_with(&at.path),
            (
                Owner::Departures(at),
                Owner::PendingDeparture(site) | Owner::ActiveDeparture(site),
            ) => at.unit == site.unit && site.path.starts_with(&at.path),
            (Owner::Sessions(_), Owner::SessionCell { .. }) => true,
            _ => false,
        };
        // A session cell a name resolves to stands only as that name's
        // alias: it is a member as the slots' own owners are.
        self.slots
            .keys()
            .chain(self.aliases.values())
            .filter(|owner| included(owner))
            .filter_map(|owner| self.root(owner, TaskId::MAIN, 0))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }
    pub(crate) fn select_group(
        &self,
        group: &Owner,
        task: TaskId,
        invocation: u64,
        candidates: &[Root],
    ) -> Option<Box<[Root]>> {
        if matches!(group, Owner::Sessions(_)) {
            return self.group(group, task, invocation);
        }
        self.with_group(group, |members| {
            candidates
                .iter()
                .copied()
                .filter(|owner| match owner {
                    Root::Value {
                        task: t,
                        invocation: i,
                        slot,
                    } => {
                        *t == task
                            && *i == invocation
                            && members
                                .binary_search(&Root::Value {
                                    task: TaskId::MAIN,
                                    invocation: 0,
                                    slot: *slot,
                                })
                                .is_ok()
                    }
                    _ => false,
                })
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        })
    }
    fn with_group<T>(&self, group: &Owner, read: impl FnOnce(&[Root]) -> T) -> Option<T> {
        if !matches!(
            group,
            Owner::Scope(_) | Owner::Operands(_) | Owner::Departures(_) | Owner::Sessions(_)
        ) {
            return None;
        }
        let mut groups = self
            .groups
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let members = groups
            .entry(group.clone())
            .or_insert_with(|| self.resolve_group(group));
        Some(read(members))
    }
}

#[cfg(test)]
mod role_key_laws {
    use super::*;

    /// Q4: a body's Slot→RoleKey table round-trips each role through its own
    /// key, and a table under which one key would name two roles refuses the
    /// body, so no frame of it can persist a slot two roles share.
    #[test]
    fn one_role_key_naming_two_roles_refuses_the_body() {
        let document = lash_kernel_doc::parse_document(
            "numbers by_spelling\nkernel 1\nmain { let a = 1 let b = [a, 2] return [a, b] }",
        )
        .unwrap();
        let source = Source::compile(
            doc::constant_pool::PoolOwner::Document(document.identity()),
            Site::new(lash_kernel_value::sites::Unit::Main, vec![]),
            document.main.as_ref().unwrap(),
            &[],
            Default::default(),
            &BTreeMap::new(),
            &BTreeSet::from([Name::new("a"), Name::new("b")]),
        );
        let table = source.owner_slots().unwrap();
        for owner in table.slots.keys().chain(table.binding_slots.keys()) {
            if let (Some(slot), Some(role)) = (table.slot(owner), source_role(owner)) {
                assert_eq!(slot.id(), role.id(), "{owner:?} is keyed by its role");
                assert_eq!(table.role(slot), Some(role), "{slot:?} names {owner:?}");
            }
        }
        let rebuilt = |slots, binding_slots| OwnerSlots {
            owner: table.owner,
            source: table.source.clone(),
            slots,
            scopes: table.scopes.clone(),
            aliases: table.aliases.clone(),
            binding_slots,
            roles: BTreeMap::new(),
            groups: Default::default(),
        };
        // Two operands under the first one's key.
        let operands: Vec<_> = table
            .slots
            .iter()
            .filter(|(owner, id)| matches!(owner, Owner::Operand(_)) && id.is_some())
            .collect();
        let [(_, key), (other, _), ..] = operands[..] else {
            panic!("the body has two operands");
        };
        let mut slots = table.slots.clone();
        slots.insert((*other).clone(), *key);
        let merged = rebuilt(slots, table.binding_slots.clone());
        assert_eq!(merged.role_table(), Err(Refused::Facts));
        // Two bindings under the first one's key.
        let binding = |name: &str, key| {
            let declared = Site::new(lash_kernel_value::sites::Unit::Main, vec![0]);
            let name = Name::new(name);
            (Owner::Binding { declared, name }, (key, false))
        };
        let key = SourceRole::Binding {
            declared: Site::new(lash_kernel_value::sites::Unit::Main, vec![0]),
            name: Name::new("a"),
        }
        .id();
        let binding_slots = BTreeMap::from([binding("a", key), binding("b", key)]);
        let merged = rebuilt(table.slots.clone(), binding_slots);
        assert_eq!(merged.role_table(), Err(Refused::Facts));
    }
}
