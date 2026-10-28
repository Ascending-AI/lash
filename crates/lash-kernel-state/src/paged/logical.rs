//! Source-owned holdings and reference-counted allocation membership.
//! Physical residency never changes these trees. An operation edits cloned
//! roots and publishes only after its complete source peak has been admitted.

use super::census_graph::visit_tree;
use super::tree::{Access, Cell, SealWork, Tree};
use super::{KeyedRoot, SealBudget};
use lash_kernel_doc::LogicalHeldV1 as Cost;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, HeapFault, Identity};
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

mod codec;
mod graph;
mod ownership;
pub use ownership::RootValue;
mod values;
pub(crate) use values::stream_value_edges;

/// An index in the exact source unit, assigned before register placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    Binding(u32),
    Operand(u32),
    Control(u32),
    Argument(u32),
    Departure(u32),
    Continuation(u32),
    Scratch(u32),
    /// A frame's borrowed reach to a captured variable cell. The declaring
    /// binding and closure entity own the priced occurrences.
    Capture(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    Frame {
        task: TaskId,
        depth: u64,
    },
    Value {
        task: TaskId,
        depth: u64,
        slot: Slot,
    },
    Session([u8; 32]),
    Incoming(TaskId),
    Ended(TaskId),
}
impl Owner {
    fn transient(self) -> bool {
        matches!(
            self,
            Self::Value {
                slot: Slot::Operand(_) | Slot::Scratch(_),
                ..
            }
        )
    }
    fn ended(self) -> bool {
        matches!(self, Self::Ended(_))
    }
}

#[derive(Clone)]
struct Edge {
    target: Identity,
    count: u64,
}

/// One immutable occurrence's complete price and mutable/task reach. Cloning
/// shares the edge index, but every owner still contributes its local price.
#[derive(Clone)]
pub struct Holding {
    bytes: u64,
    links: Tree<Edge>,
}
impl std::fmt::Debug for Holding {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        out.debug_struct("Holding")
            .field("bytes", &self.bytes)
            .field("identities", &self.links.len())
            .finish()
    }
}
impl Holding {
    pub fn scalar(bytes: u64) -> Self {
        Self {
            bytes,
            links: Tree::new(false),
        }
    }
    pub fn frame() -> Self {
        Self::scalar(Cost::FRAME)
    }
    pub fn bytes(&self) -> u64 {
        self.bytes
    }
    pub fn with_edges(
        bytes: u64,
        edges: impl IntoIterator<Item = Identity>,
        access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let mut holding = Self::scalar(bytes);
        for target in edges {
            holding.add_edge(target, 1, access)?;
        }
        Ok(holding)
    }
    fn add_edge(
        &mut self,
        target: Identity,
        amount: u64,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        let key = identity_key(target);
        let count = self
            .links
            .get(&key, access)?
            .map_or(0, |edge| edge.count)
            .checked_add(amount)
            .ok_or_else(|| invalid("edge count overflow"))?;
        self.links.write(key, Edge { target, count }, access)
    }
    fn each_edge(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&Edge) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        visit_tree(&self.links, access, visit)
    }
}

#[derive(Clone)]
struct RootHolding {
    owner: Owner,
    value: Holding,
}
#[derive(Clone)]
struct EntityHolding {
    body: Holding,
    incoming: u64,
}

/// These are current components, never a saved maximum or a residency floor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Balance {
    pub entity_local_bytes: u64,
    pub root_and_control_bytes: u64,
    pub entities: u64,
}
impl Balance {
    pub fn total(self) -> Result<u64, HeapFault> {
        self.entity_local_bytes
            .checked_add(self.root_and_control_bytes)
            .ok_or_else(|| invalid("total holding cost overflow"))
    }
}

/// All membership and owner itemization needed by a cold activation. Sealing
/// changes only dirty paths; opening these roots does not traverse holdings.
#[derive(Clone, Copy, Debug)]
pub struct LedgerRoot {
    pub owners: KeyedRoot,
    pub ended: KeyedRoot,
    pub entities: KeyedRoot,
    pub refinements: u64,
}

#[derive(Clone)]
struct State {
    owners: Tree<RootHolding>,
    ended: Tree<RootHolding>,
    entities: Tree<EntityHolding>,
    refinements: u64,
}
impl State {
    fn balance(&self) -> Result<Balance, HeapFault> {
        let balance = Balance {
            entity_local_bytes: self.entities.cost(),
            root_and_control_bytes: self
                .owners
                .cost()
                .checked_add(self.ended.cost())
                .ok_or_else(|| invalid("root holding cost overflow"))?,
            entities: self.entities.len(),
        };
        balance.total()?;
        Ok(balance)
    }
    fn roots(&self, owner: Owner) -> &Tree<RootHolding> {
        if owner.ended() {
            &self.ended
        } else {
            &self.owners
        }
    }
    fn roots_mut(&mut self, owner: Owner) -> &mut Tree<RootHolding> {
        if owner.ended() {
            &mut self.ended
        } else {
            &mut self.owners
        }
    }
}

/// Source changes within one indivisible event, in source evaluation order.
/// A Roots change transfers ownership atomically: new edges are retained
/// before old ones are released, including self-aliasing replacements.
pub enum Change {
    Roots(Vec<(Owner, Option<Holding>)>),
    /// Move the exact existing occurrence, replacing the destination atomically.
    Move {
        from: Owner,
        to: Owner,
    },
    /// A source read creates another occurrence with the same identity reach.
    Copy {
        from: Owner,
        to: Owner,
    },
    Allocate {
        entity: EntityId,
        body: Holding,
    },
    Mutate {
        entity: EntityId,
        body: Holding,
    },
}

pub struct Ledger {
    state: State,
    identity: Rc<()>,
    revision: u64,
}
/// A complete staged operation. No semantic tree has changed yet.
pub struct Prepared {
    identity: Rc<()>,
    revision: u64,
    next: State,
    peak: u64,
    before: u64,
}
pub struct Accepted(Prepared);
impl Prepared {
    pub fn peak(&self) -> u64 {
        self.peak
    }
    pub fn balance(&self) -> Result<Balance, HeapFault> {
        self.next.balance()
    }
    /// A checked source description can require more room than the published
    /// delta, for elided frames and temporary holdings. It can only increase
    /// this operation's independently computed minimum peak.
    pub fn require_source_room(mut self, extra: u64) -> Option<Self> {
        self.peak = self.peak.max(self.before.checked_add(extra)?);
        Some(self)
    }
    pub fn admit(self, bound: u64) -> Result<Accepted, Box<Self>> {
        if self.peak <= bound {
            Ok(Accepted(self))
        } else {
            Err(Box::new(self))
        }
    }
}

impl Ledger {
    pub fn fresh() -> Self {
        Self {
            state: State {
                owners: Tree::new(false),
                ended: Tree::new(false),
                entities: Tree::new(false),
                refinements: 0,
            },
            identity: Rc::new(()),
            revision: 0,
        }
    }
    pub fn open(root: LedgerRoot) -> Result<Self, HeapFault> {
        let ledger = Self {
            state: State {
                owners: Tree::of_keyed(&root.owners),
                ended: Tree::of_keyed(&root.ended),
                entities: Tree::of_keyed(&root.entities),
                refinements: root.refinements,
            },
            identity: Rc::new(()),
            revision: 0,
        };
        ledger.balance()?;
        Ok(ledger)
    }
    pub fn balance(&self) -> Result<Balance, HeapFault> {
        self.state.balance()
    }
    /// A callback, cold load or safe-point handoff expires outstanding
    /// capabilities even when no logical holding changes. Price stays constant.
    pub fn handoff(&mut self) -> Result<(), HeapFault> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("logical revision overflow"))?;
        Ok(())
    }
    pub fn holding(&self, owner: Owner, access: &Access<'_>) -> Result<Option<Holding>, HeapFault> {
        Ok(self
            .state
            .roots(owner)
            .get(&owner_key(owner), access)?
            .map(|v| v.value))
    }
    pub fn prepare(
        &self,
        changes: impl IntoIterator<Item = Change>,
        access: &Access<'_>,
    ) -> Result<Prepared, HeapFault> {
        let before = self.balance()?.total()?;
        let mut edit = Edit {
            state: self.state.clone(),
            access,
            zero: VecDeque::new(),
            fresh: Vec::new(),
        };
        let mut peak = before;
        for change in changes {
            match change {
                Change::Roots(changes) => edit.roots(changes)?,
                Change::Move { from, to } => edit.transfer(from, to, false)?,
                Change::Copy { from, to } => edit.transfer(from, to, true)?,
                Change::Allocate { entity, body } => {
                    if body.bytes < Cost::OBJECT {
                        return Err(invalid("entity has no logical header"));
                    }
                    let key = entity_key(entity);
                    if edit.state.entities.get(&key, access)?.is_some() {
                        return Err(invalid("entity already allocated"));
                    }
                    edit.state.entities.write(
                        key,
                        EntityHolding {
                            body: body.clone(),
                            incoming: 0,
                        },
                        access,
                    )?;
                    edit.retain(&body)?;
                    edit.fresh.push(entity);
                }
                Change::Mutate { entity, body } => {
                    if body.bytes < Cost::OBJECT {
                        return Err(invalid("entity has no logical header"));
                    }
                    let key = entity_key(entity);
                    let mut record = edit
                        .state
                        .entities
                        .get(&key, access)?
                        .ok_or_else(|| invalid("mutated entity is absent"))?;
                    let old = std::mem::replace(&mut record.body, body.clone());
                    edit.state.entities.write(key, record, access)?;
                    edit.retain(&body)?;
                    edit.release(&old, false)?;
                }
            }
            peak = peak.max(edit.state.balance()?.total()?);
        }
        edit.zero.extend(std::mem::take(&mut edit.fresh));
        edit.release_zero(false)?;
        Ok(Prepared {
            identity: Rc::clone(&self.identity),
            revision: self.revision,
            next: edit.state,
            peak,
            before,
        })
    }
    pub fn commit(&mut self, accepted: Accepted) -> Result<(), HeapFault> {
        let Prepared {
            identity,
            revision,
            next,
            ..
        } = accepted.0;
        if !Rc::ptr_eq(&identity, &self.identity) || revision != self.revision {
            return Err(invalid("stale logical admission"));
        }
        let revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("logical revision overflow"))?;
        self.state = next;
        self.revision = revision;
        Ok(())
    }
    pub fn seal(
        &mut self,
        access: &Access<'_>,
        budget: SealBudget,
    ) -> Result<Option<LedgerRoot>, HeapFault> {
        if self.state.owners.live_members(access)? != 0 {
            return Err(invalid("transient source owners at seal"));
        }
        let mut state = self.state.clone();
        let mut work = SealWork::new(budget);
        if !state.owners.seal(access, &mut work)?
            || !state.ended.seal(access, &mut work)?
            || !state.entities.seal(access, &mut work)?
        {
            return Ok(None);
        }
        let root = LedgerRoot {
            owners: keyed(&state.owners)?,
            ended: keyed(&state.ended)?,
            entities: keyed(&state.entities)?,
            refinements: state.refinements,
        };
        self.state = state;
        Ok(Some(root))
    }
}

struct Edit<'a, 'port> {
    state: State,
    access: &'a Access<'port>,
    zero: VecDeque<EntityId>,
    fresh: Vec<EntityId>,
}
impl Edit<'_, '_> {
    fn retain(&mut self, value: &Holding) -> Result<(), HeapFault> {
        let mut edges = Vec::new();
        value.each_edge(self.access, &mut |edge| {
            edges.push(edge.clone());
            Ok(())
        })?;
        for edge in edges {
            if let Identity::Object(entity) = edge.target {
                let key = entity_key(entity);
                let mut record = self
                    .state
                    .entities
                    .get(&key, self.access)?
                    .ok_or_else(|| invalid("holding reaches absent entity"))?;
                record.incoming = record
                    .incoming
                    .checked_add(edge.count)
                    .ok_or_else(|| invalid("incoming ownership overflow"))?;
                self.state.entities.write(key, record, self.access)?;
            }
        }
        Ok(())
    }
    fn release(&mut self, value: &Holding, allow_swept: bool) -> Result<(), HeapFault> {
        let mut edges = Vec::new();
        value.each_edge(self.access, &mut |edge| {
            edges.push(edge.clone());
            Ok(())
        })?;
        for edge in edges {
            if let Identity::Object(entity) = edge.target {
                let key = entity_key(entity);
                let Some(mut record) = self.state.entities.get(&key, self.access)? else {
                    if allow_swept {
                        continue;
                    }
                    return Err(invalid("released entity is absent"));
                };
                record.incoming = record
                    .incoming
                    .checked_sub(edge.count)
                    .ok_or_else(|| invalid("incoming ownership underflow"))?;
                if record.incoming == 0 {
                    self.zero.push_back(entity);
                }
                self.state.entities.write(key, record, self.access)?;
            }
        }
        Ok(())
    }
    fn release_zero(&mut self, allow_swept: bool) -> Result<(), HeapFault> {
        while let Some(entity) = self.zero.pop_front() {
            let key = entity_key(entity);
            let Some(record) = self.state.entities.get(&key, self.access)? else {
                continue;
            };
            if record.incoming != 0 {
                continue;
            }
            self.state.entities.remove(&key, self.access)?;
            self.release(&record.body, allow_swept)?;
        }
        Ok(())
    }
    fn transfer(&mut self, from: Owner, to: Owner, copy: bool) -> Result<(), HeapFault> {
        let value = self
            .state
            .roots(from)
            .get(&owner_key(from), self.access)?
            .ok_or_else(|| invalid("source transfer has no owner"))?
            .value;
        if from == to {
            return if copy {
                Err(invalid("source copy has no distinct destination"))
            } else {
                Ok(())
            };
        }
        let mut changes = vec![(to, Some(value))];
        if !copy {
            changes.push((from, None));
        }
        self.roots(changes)
    }
    fn roots(&mut self, changes: Vec<(Owner, Option<Holding>)>) -> Result<(), HeapFault> {
        let mut old = Vec::new();
        let mut unique = BTreeMap::new();
        for (owner, value) in changes {
            if value.is_none()
                && self
                    .state
                    .roots(owner)
                    .get(&owner_key(owner), self.access)?
                    .is_none()
            {
                return Err(invalid("released source owner is absent"));
            }
            if unique.insert(owner, value).is_some() {
                return Err(invalid("owner replaced twice in one transfer"));
            }
        }
        for (owner, value) in &unique {
            if let Some(value) = value {
                codec::validate_root(&RootHolding {
                    owner: *owner,
                    value: value.clone(),
                })?;
                if matches!(
                    owner,
                    Owner::Value {
                        slot: Slot::Capture(_),
                        ..
                    }
                ) {
                    value.each_edge(self.access, &mut |edge| {
                        if edge.count != 1 || !matches!(edge.target, Identity::Object(_)) {
                            return Err(invalid("capture must borrow one variable cell"));
                        }
                        Ok(())
                    })?;
                }
                self.retain(value)?;
            }
        }
        for (owner, value) in unique {
            let key = owner_key(owner);
            if let Some(previous) = self.state.roots(owner).get(&key, self.access)? {
                old.push(previous.value);
            }
            match value {
                Some(value) => {
                    self.state.roots_mut(owner).write(
                        key,
                        RootHolding { owner, value },
                        self.access,
                    )?;
                }
                None => {
                    self.state.roots_mut(owner).remove(&key, self.access)?;
                }
            }
        }
        for value in old {
            self.release(&value, false)?;
        }
        self.release_zero(false)
    }
}

fn invalid(reason: &str) -> HeapFault {
    HeapFault::Malformed(reason.into())
}
fn entity_key(entity: EntityId) -> Vec<u8> {
    [entity.family.0.to_be_bytes(), entity.ordinal.to_be_bytes()].concat()
}
fn identity_key(identity: Identity) -> Vec<u8> {
    match identity {
        Identity::Object(entity) => [vec![0], entity_key(entity)].concat(),
        Identity::Task(task) => [vec![1], task.0.to_be_bytes().to_vec()].concat(),
    }
}
fn owner_key(owner: Owner) -> Vec<u8> {
    codec::owner_key(owner)
}
fn keyed<T: Cell>(tree: &Tree<T>) -> Result<KeyedRoot, HeapFault> {
    super::heap_impl::keyed(tree)
}
