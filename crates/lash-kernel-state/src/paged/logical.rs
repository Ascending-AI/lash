//! Source-owned holdings and reference-counted allocation membership.
//! Physical residency never changes these trees. An operation edits cloned
//! roots and publishes only after its complete source peak has been admitted.

use super::tree::{Access, Cell, SealWork};
mod index;
use super::{KeyedRoot, SealBudget};
use index::{Index as Tree, visit_tree};
mod links;
use lash_kernel_doc::LogicalHeldV1 as Cost;
use lash_kernel_doc::TaskId;
use lash_kernel_doc::heap::{EntityId, FamilyId, HeapFault, Identity};
use links::Links;
use std::collections::{BTreeMap, VecDeque};
use std::rc::Rc;

mod codec;
mod dependencies;
mod facts;
mod member_batch;
pub use dependencies::Dependencies;
mod graph;
mod invocations;
pub use invocations::{Invocation, InvocationRange};
mod boundaries;
mod members;
mod observers;
pub use observers::ObserverExclusion;
mod ownership;
mod program;
#[cfg(any(test, feature = "perf-witness"))]
pub use program::{
    AcceptedProgram, EvaluatedProgram, JournalEvent, Materializations, PreparedProgram, Program,
};
pub use program::{FrameEvent, ScratchEvent, VirtualBody, VirtualEntity};
mod publication;
mod region;
use members::PreparedMember;
pub use members::{Member, MemberFault};
pub use ownership::{PublicationValue, RootValue};
pub use publication::{AcceptedBoundary, BoundaryTransaction, MemberEdit};
#[cfg(any(test, feature = "perf-witness"))]
pub use region::fixtures as region_fixtures;
pub use region::{
    LeaseAllocation, LeaseCallable, LeaseFacts, LeaseFault, LeaseGraphExit, LeaseMeasure,
    LeaseMember, LeaseObject, LeaseOp, LeaseStep, LeaseValue, PreparedStateExit, ReadyLeaseStep,
    StateExit, StateLease,
};
mod warming;
pub use warming::WarmFact;
mod values;
pub(crate) use values::stream_value_edges;

/// An index in the exact source unit, assigned before register placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Owner {
    Frame {
        task: TaskId,
        invocation: u64,
    },
    Value {
        task: TaskId,
        invocation: u64,
        slot: Slot,
    },
    Session([u8; 32]),
    Saved([u8; 32]),
    Incoming(TaskId),
    Ended(TaskId),
}
impl Owner {
    pub fn session(name: &lash_kernel_value::Name) -> Self {
        Self::Session(lash_kernel_value::tokens::logical_digest(
            &lash_kernel_value::tokens::encode_body(name),
        ))
    }
    pub fn saved(name: &lash_kernel_value::Name) -> Self {
        let Self::Session(key) = Self::session(name) else {
            unreachable!()
        };
        Self::Saved(key)
    }
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
    links: Links,
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
            links: Links::Empty,
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
        _access: &Access<'_>,
    ) -> Result<Self, HeapFault> {
        let mut batch = links::EdgeBatch::default();
        for target in edges {
            batch.push(target, 1)?;
        }
        Ok(Self {
            bytes,
            links: batch.finish()?,
        })
    }
    fn add_edge(
        &mut self,
        target: Identity,
        amount: u64,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        let key = identity_key(target);
        self.links
            .write_with(&key, access, |old| {
                let count = old
                    .map_or(0, |edge| edge.count)
                    .checked_add(amount)
                    .ok_or_else(|| invalid("edge count overflow"))?;
                Ok::<_, HeapFault>(Some(Edge { target, count }))
            })
            .map(|_| ())
    }
    fn replace_members(
        &mut self,
        remove: &Self,
        add: &Self,
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        self.bytes = self
            .bytes
            .checked_sub(remove.bytes)
            .and_then(|bytes| bytes.checked_add(add.bytes))
            .filter(|bytes| *bytes >= Cost::OBJECT)
            .ok_or_else(|| invalid("member cost overflow or missing header"))?;
        remove.each_edge(access, &mut |edge| {
            let key = identity_key(edge.target);
            let old = self
                .links
                .get(&key, access)?
                .ok_or_else(|| invalid("removed member edge is absent"))?;
            let count = old
                .count
                .checked_sub(edge.count)
                .ok_or_else(|| invalid("member edge underflow"))?;
            if count == 0 {
                self.links.remove(&key, access)?;
            } else {
                self.links.write(
                    key,
                    Edge {
                        target: edge.target,
                        count,
                    },
                    access,
                )?;
            }
            Ok(())
        })?;
        add.each_edge(access, &mut |edge| {
            self.add_edge(edge.target, edge.count, access)
        })?;
        Ok(())
    }
    /// Whether an edge names a provisional ledger key, which a projection
    /// rekeys to a durable id.
    pub fn has_provisional_edges(&self, access: &Access<'_>) -> Result<bool, HeapFault> {
        if let links::Links::Many(index) = &self.links {
            return Ok(index
                .hot_rows()
                .iter()
                .any(|(_, edge)| provisional(edge.target)));
        }
        let mut found = false;
        self.each_edge(access, &mut |edge| {
            found |= provisional(edge.target);
            Ok(())
        })?;
        Ok(found)
    }
    /// This holding with each edge `rekeyed` names moved to its durable
    /// id, or None when no edge moves.
    fn rekeyed(
        &self,
        rekeyed: &std::collections::HashMap<EntityId, EntityId>,
        access: &Access<'_>,
    ) -> Result<Option<Self>, HeapFault> {
        if let links::Links::Many(index) = &self.links {
            let mut moves = Vec::new();
            for (_, edge) in index.hot_rows() {
                if let Identity::Object(entity) = edge.target
                    && let Some(durable) = rekeyed.get(&entity)
                {
                    moves.push((
                        edge.target,
                        Edge {
                            target: Identity::Object(*durable),
                            count: edge.count,
                        },
                    ));
                }
            }
            if moves.is_empty() {
                return Ok(None);
            }
            let mut next = index.as_ref().clone();
            for (from, _) in &moves {
                next.remove(&identity_key(*from), access)?;
            }
            for (_, mut edge) in moves {
                let key = identity_key(edge.target);
                if let Some(previous) = next.get(&key, access)? {
                    edge.count = edge
                        .count
                        .checked_add(previous.count)
                        .ok_or_else(|| invalid("edge count overflow"))?;
                }
                next.write(key, edge, access)?;
            }
            return Ok(Some(Self {
                bytes: self.bytes,
                links: links::Links::Many(Rc::new(next)),
            }));
        }
        let mut moved = false;
        let mut batch = links::EdgeBatch::default();
        self.each_edge(access, &mut |edge| {
            let target = match edge.target {
                Identity::Object(entity) => match rekeyed.get(&entity) {
                    Some(durable) => {
                        moved = true;
                        Identity::Object(*durable)
                    }
                    None => edge.target,
                },
                target => target,
            };
            batch.push(target, edge.count)
        })?;
        if !moved {
            return Ok(None);
        }
        Ok(Some(Self {
            bytes: self.bytes,
            links: batch.finish()?,
        }))
    }
    pub fn visit_edges(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(Identity, u64) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.each_edge(access, &mut |edge| visit(edge.target, edge.count))
    }
    fn each_edge(
        &self,
        access: &Access<'_>,
        visit: &mut dyn FnMut(&Edge) -> Result<(), HeapFault>,
    ) -> Result<(), HeapFault> {
        self.links.each(access, visit)
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
    pub execution: KeyedRoot,
    pub ended: KeyedRoot,
    pub entities: KeyedRoot,
    pub refinements: u64,
    pub next_invocation: u64,
}

impl LedgerRoot {
    /// Authenticated source balance from root summaries; no physical residency
    /// counter or previous capacity certificate participates in this value.
    pub fn balance(self) -> Result<Balance, HeapFault> {
        let balance = Balance {
            entity_local_bytes: self.entities.cost().0,
            root_and_control_bytes: self
                .owners
                .cost()
                .0
                .checked_add(self.execution.cost().0)
                .and_then(|cost| cost.checked_add(self.ended.cost().0))
                .ok_or_else(|| invalid("root holding cost overflow"))?,
            entities: self.entities.len(),
        };
        balance.total()?;
        Ok(balance)
    }
}

#[derive(Clone)]
struct State {
    owners: Tree<RootHolding>,
    execution: Tree<RootHolding>,
    ended: Tree<RootHolding>,
    entities: Tree<EntityHolding>,
    refinements: u64,
    next_invocation: u64,
}
impl State {
    /// A placeholder held only while the committed state is edited in place.
    fn vacant() -> Self {
        Self {
            owners: Tree::new(false),
            execution: Tree::new(false),
            ended: Tree::new(false),
            entities: Tree::new(false),
            refinements: 0,
            next_invocation: 0,
        }
    }
    fn balance(&self) -> Result<Balance, HeapFault> {
        let balance = Balance {
            entity_local_bytes: self.entities.cost(),
            root_and_control_bytes: self
                .owners
                .cost()
                .checked_add(self.execution.cost())
                .and_then(|cost| cost.checked_add(self.ended.cost()))
                .ok_or_else(|| invalid("root holding cost overflow"))?,
            entities: self.entities.len(),
        };
        balance.total()?;
        Ok(balance)
    }
    fn roots(&self, owner: Owner) -> &Tree<RootHolding> {
        if owner.ended() {
            &self.ended
        } else if matches!(owner, Owner::Session(_) | Owner::Saved(_)) {
            &self.owners
        } else {
            &self.execution
        }
    }
    fn roots_mut(&mut self, owner: Owner) -> &mut Tree<RootHolding> {
        if owner.ended() {
            &mut self.ended
        } else if matches!(owner, Owner::Session(_) | Owner::Saved(_)) {
            &mut self.owners
        } else {
            &mut self.execution
        }
    }
}

/// Source changes in source evaluation order. Each entry is a frontier unless
/// assembled by a checked indivisible-event builder such as prepare_ownership.
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
    /// A checked member edit changes only its local occurrences and edges.
    Members {
        entity: EntityId,
        remove: Holding,
        add: Box<Holding>,
    },
}

pub struct Ledger {
    state: State,
    identity: Rc<()>,
    revision: u64,
    facts: std::cell::RefCell<facts::Facts>,
}
/// A process-local dependency stamp. Every committed owner, member, departure
/// or normalization change invalidates it. It cannot authenticate another run
/// or survive opening a sealed root. Arena facts carry their own content stamps.
#[derive(Clone)]
pub struct Stamp {
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
    projected: Vec<EntityId>,
}
pub struct Accepted(Prepared);
impl Prepared {
    /// Exact source terminal normalization stays in the unpublished draft.
    /// A trace failure publishes neither root replacement nor partial sweep.
    pub fn normalize(
        mut self,
        access: &Access<'_>,
        limits: crate::paged::census_impl::TraceLimits,
    ) -> Result<Self, HeapFault> {
        let mut draft = Ledger {
            state: self.next,
            identity: Rc::clone(&self.identity),
            revision: self.revision,
            facts: Default::default(),
        };
        draft.normalize(access, limits)?;
        self.next = draft.state;
        Ok(self)
    }
    pub fn peak(&self) -> u64 {
        self.peak
    }
    /// Collect what `before` already held unreachable, then normalize the
    /// draft. No event of the journal reaches that garbage, so it stays
    /// unreachable through every step: the live peak is exactly the
    /// journal's peak less its cost.
    pub fn collect(
        self,
        before: &Ledger,
        access: &Access<'_>,
        limits: crate::paged::census_impl::TraceLimits,
    ) -> Result<Self, HeapFault> {
        let mut swept = Ledger {
            state: before.state.clone(),
            identity: Rc::new(()),
            revision: before.revision,
            facts: Default::default(),
        };
        swept.normalize(access, limits)?;
        let garbage = before
            .balance()?
            .total()?
            .checked_sub(swept.balance()?.total()?)
            .ok_or_else(|| invalid("normalization grew the ledger"))?;
        let mut collected = self.normalize(access, limits)?;
        collected.peak = collected
            .peak
            .checked_sub(garbage)
            .ok_or_else(|| invalid("garbage exceeds the journal peak"))?;
        collected.before = collected
            .before
            .checked_sub(garbage)
            .ok_or_else(|| invalid("garbage exceeds the prior balance"))?;
        Ok(collected)
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
    #[cfg(test)]
    pub(crate) fn leave_one_publication_for_law(&mut self) {
        self.revision = u64::MAX - 1;
    }
    pub fn fresh() -> Self {
        Self {
            state: State {
                owners: Tree::new(false),
                execution: Tree::new(false),
                ended: Tree::new(false),
                entities: Tree::new(false),
                refinements: 0,
                next_invocation: 1,
            },
            identity: Rc::new(()),
            revision: 0,
            facts: Default::default(),
        }
    }
    pub fn open(root: LedgerRoot) -> Result<Self, HeapFault> {
        let ledger = Self {
            state: State {
                owners: Tree::of_keyed(&root.owners),
                execution: Tree::of_keyed(&root.execution),
                ended: Tree::of_keyed(&root.ended),
                entities: Tree::of_keyed(&root.entities),
                refinements: root.refinements,
                next_invocation: root.next_invocation,
            },
            identity: Rc::new(()),
            revision: 0,
            facts: Default::default(),
        };
        ledger.balance()?;
        Ok(ledger)
    }
    pub fn balance(&self) -> Result<Balance, HeapFault> {
        self.state.balance()
    }
    pub fn stamp(&self) -> Stamp {
        Stamp {
            identity: Rc::clone(&self.identity),
            revision: self.revision,
        }
    }
    /// A settled affine charge receipt may outlive root revisions, but never
    /// its ledger. This identity check grants no root or content authority.
    pub fn owns_stamp(&self, stamp: &Stamp) -> bool {
        Rc::ptr_eq(&self.identity, &stamp.identity)
    }
    pub fn is_current(&self, stamp: &Stamp) -> bool {
        Rc::ptr_eq(&self.identity, &stamp.identity) && self.revision == stamp.revision
    }
    pub fn entity_holding(
        &self,
        entity: EntityId,
        access: &Access<'_>,
    ) -> Result<Option<Holding>, HeapFault> {
        Ok(self
            .state
            .entities
            .get(&entity_key(entity), access)?
            .map(|entity| entity.body))
    }
    /// The exact source-retained release of a group at this frontier. Aliases
    /// release a body once; a cycle remains itemized until normalization.
    pub fn release_cost(&self, owners: &[Owner], access: &Access<'_>) -> Result<u64, HeapFault> {
        let before = self.balance()?.total()?;
        let prepared = self.prepare(
            [Change::Roots(
                owners.iter().map(|owner| (*owner, None)).collect(),
            )],
            access,
        )?;
        before
            .checked_sub(prepared.balance()?.total()?)
            .ok_or_else(|| invalid("release increased holdings"))
    }
    pub fn prepare_at(
        &self,
        stamp: &Stamp,
        changes: impl IntoIterator<Item = Change>,
        access: &Access<'_>,
    ) -> Result<Prepared, HeapFault> {
        if !self.is_current(stamp) {
            return Err(invalid("stale source facts"));
        }
        self.prepare(changes, access)
    }
    /// A callback, cold load or safe-point handoff expires outstanding
    /// capabilities even when no logical holding changes. Price stays constant.
    pub fn handoff(&mut self) -> Result<(), HeapFault> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or_else(|| invalid("logical revision overflow"))?;
        self.facts.get_mut().clear();
        Ok(())
    }
    /// Commit one proven release of present, distinct owners in place,
    /// through the same graph edit as a checked journal: each owner's edges
    /// are released once and every entity left unowned is retired. A missing
    /// or repeated owner declines (`false`) before any edit. Outstanding
    /// admissions expire.
    /// Whether every object `value` references is a committed entity, so a
    /// region record holding it publishes onto entities that still exist.
    pub fn references_committed(
        &self,
        value: &Holding,
        access: &Access<'_>,
    ) -> Result<bool, HeapFault> {
        let mut committed = true;
        value.each_edge(access, &mut |edge| {
            if committed && let Identity::Object(entity) = edge.target {
                committed = self
                    .state
                    .entities
                    .get_with(&entity_key(entity), access, |row| row.is_some())?;
            }
            Ok(())
        })?;
        Ok(committed)
    }
    pub fn release(&mut self, owners: &[Owner], access: &Access<'_>) -> Result<bool, HeapFault> {
        for (index, owner) in owners.iter().enumerate() {
            if owners[..index].contains(owner) || !self.present(*owner, access)? {
                return Ok(false);
            }
        }
        self.edit_in_place(access, owners.iter().map(|owner| (*owner, None)).collect())?;
        Ok(true)
    }
    /// Commit one proven move of a present owner's holding into another owner
    /// in place, releasing the destination's previous holding, through the
    /// same graph edit as a checked journal. An absent source or a move onto
    /// itself declines (`false`) before any edit. Outstanding admissions expire.
    pub fn transfer(
        &mut self,
        from: Owner,
        to: Owner,
        access: &Access<'_>,
    ) -> Result<bool, HeapFault> {
        let Some(row) = self.state.roots(from).get(&owner_key(from), access)? else {
            return Ok(false);
        };
        if from == to {
            return Ok(false);
        }
        self.edit_in_place(access, vec![(to, Some(row.value)), (from, None)])?;
        Ok(true)
    }
    fn present(&self, owner: Owner, access: &Access<'_>) -> Result<bool, HeapFault> {
        self.state
            .roots(owner)
            .get_with(&owner_key(owner), access, |row| row.is_some())
    }
    /// The committed state is edited directly: its trees are uniquely owned,
    /// so copy-on-write copies no search path. A fault mid-edit still expires
    /// every outstanding admission.
    fn edit_in_place(
        &mut self,
        access: &Access<'_>,
        changes: Vec<(Owner, Option<Holding>)>,
    ) -> Result<(), HeapFault> {
        self.handoff()?;
        let mut edit = Edit {
            state: std::mem::replace(&mut self.state, State::vacant()),
            access,
            zero: VecDeque::new(),
            fresh: Vec::new(),
        };
        let edited = edit.roots(changes);
        self.state = edit.state;
        edited
    }
    pub fn prepare(
        &self,
        changes: impl IntoIterator<Item = Change>,
        access: &Access<'_>,
    ) -> Result<Prepared, HeapFault> {
        self.prepare_changes(changes, access, true)
    }
    /// Headers, member edges and consumed operands are mechanical parts of
    /// one source event. Their staging order creates no guest-visible peak.
    fn prepare_event(
        &self,
        changes: impl IntoIterator<Item = Change>,
        access: &Access<'_>,
    ) -> Result<Prepared, HeapFault> {
        self.prepare_changes(changes, access, false)
    }
    fn prepare_changes(
        &self,
        changes: impl IntoIterator<Item = Change>,
        access: &Access<'_>,
        frontiers: bool,
    ) -> Result<Prepared, HeapFault> {
        let before = self.balance()?.total()?;
        let mut edit = Edit {
            state: self.state.clone(),
            access,
            zero: VecDeque::new(),
            fresh: Vec::new(),
        };
        let mut peak = before;
        let mut projected = Vec::new();
        let mut changes = changes.into_iter().peekable();
        while let Some(change) = changes.next() {
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
                    projected.push(entity);
                }
                Change::Members {
                    entity,
                    remove,
                    add,
                } => {
                    if !frontiers && changes.size_hint().0 >= 32 {
                        edit.member_batch(entity, remove, *add, &mut changes)?;
                        continue;
                    }
                    let key = entity_key(entity);
                    if remove.bytes == add.bytes
                        && remove.links.is_empty()
                        && add.links.is_empty()
                        && edit.state.entities.get_with(&key, access, |record| {
                            record.is_some_and(|record| record.body.bytes >= remove.bytes)
                        })?
                    {
                        continue;
                    }
                    edit.state.entities.update(key, access, |record| {
                        record.body.replace_members(&remove, &add, access)?;
                        Ok(true)
                    })?;
                    edit.retain(&add)?;
                    edit.release(&remove, false)?;
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
            if frontiers {
                peak = peak.max(edit.state.balance()?.total()?);
            }
        }
        edit.zero.extend(std::mem::take(&mut edit.fresh));
        edit.release_zero(false)?;
        peak = peak.max(edit.state.balance()?.total()?);
        Ok(Prepared {
            identity: Rc::clone(&self.identity),
            revision: self.revision,
            next: edit.state,
            peak,
            before,
            projected,
        })
    }
    pub fn commit(&mut self, accepted: Accepted) -> Result<(), HeapFault> {
        self.publish(accepted, || {})
    }
    /// Publish a fully staged semantic edit with the matching ledger journal.
    /// All fallible preparation, including shared charge admission, must finish
    /// before this call. Validate before invoking the infallible publication;
    /// no allocation or fallible check follows the semantic edit. The callback
    /// cannot borrow this ledger to advance it between validation and commit.
    pub fn publish(
        &mut self,
        accepted: Accepted,
        semantic: impl FnOnce(),
    ) -> Result<(), HeapFault> {
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
        self.facts.get_mut().clear();
        semantic();
        self.state = next;
        self.revision = revision;
        Ok(())
    }
    /// Rekeys what the ledger priced under provisional keys to the durable
    /// ids a projection gave those objects (`rekeyed`, provisional to
    /// durable). Only rows written since the last seal can name a
    /// provisional key, so only those are read. Outstanding admissions
    /// expire: they name the old keys.
    pub fn rekey(
        &mut self,
        rekeyed: &[(EntityId, EntityId)],
        access: &Access<'_>,
    ) -> Result<(), HeapFault> {
        if rekeyed.is_empty() {
            return Ok(());
        }
        self.handoff()?;
        let durable: std::collections::HashMap<EntityId, EntityId> =
            rekeyed.iter().copied().collect();
        let state = &mut self.state;
        for tree in [&mut state.owners, &mut state.execution, &mut state.ended] {
            for (key, row) in tree.hot_rows() {
                if let Some(value) = row.value.rekeyed(&durable, access)? {
                    tree.write(
                        key,
                        RootHolding {
                            owner: row.owner,
                            value,
                        },
                        access,
                    )?;
                }
            }
        }
        let mut moves = Vec::new();
        for (key, mut row) in state.entities.hot_rows() {
            let body = row.body.rekeyed(&durable, access)?;
            let moved = entity_of_key(&key).and_then(|entity| durable.get(&entity).copied());
            if body.is_none() && moved.is_none() {
                continue;
            }
            if let Some(body) = body {
                row.body = body;
            }
            match moved {
                Some(entity) => {
                    state.entities.remove(&key, access)?;
                    moves.push((entity_key(entity), row));
                }
                None => state.entities.write(key, row, access)?,
            }
        }
        for (key, row) in moves {
            if state.entities.get(&key, access)?.is_some() {
                return Err(invalid("a durable id is already a ledger key"));
            }
            state.entities.write(key, row, access)?;
        }
        Ok(())
    }
    /// A seal saves durable ids only. The projection before it gave every
    /// object a root reaches its durable id, so an entity still keyed
    /// provisionally is garbage the ledger priced: a cycle that died
    /// between projections, which no release can reach. It drops here
    /// with what only it held, as normalization would drop it, and never
    /// takes an id. A root or durable body still naming a provisional key
    /// is an object the ledger priced that no projection identified, and
    /// the seal refuses it.
    fn drop_provisional(&mut self, access: &Access<'_>) -> Result<(), HeapFault> {
        let state = &self.state;
        for tree in [&state.owners, &state.execution, &state.ended] {
            for (_, row) in tree.hot_rows() {
                if row.value.has_provisional_edges(access)? {
                    return Err(invalid("a provisional ledger key reached a seal"));
                }
            }
        }
        let mut garbage = Vec::new();
        for (key, row) in state.entities.hot_rows() {
            if entity_of_key(&key).is_some_and(|entity| entity.family == FamilyId::PROVISIONAL) {
                garbage.push(key);
            } else if row.body.has_provisional_edges(access)? {
                return Err(invalid("a provisional ledger key reached a seal"));
            }
        }
        if garbage.is_empty() {
            return Ok(());
        }
        let mut edit = Edit {
            state: self.state.clone(),
            access,
            zero: VecDeque::new(),
            fresh: Vec::new(),
        };
        let mut removed = Vec::new();
        for key in garbage {
            if let Some(row) = edit.state.entities.remove(&key, access)? {
                removed.push(row.body);
            }
        }
        for body in removed {
            edit.release(&body, true)?;
        }
        edit.release_zero(true)?;
        self.state = edit.state;
        Ok(())
    }
    pub fn seal(
        &mut self,
        access: &Access<'_>,
        budget: SealBudget,
    ) -> Result<Option<LedgerRoot>, HeapFault> {
        self.seal_with(access, &mut SealWork::new(budget))
    }
    pub(super) fn sealed_root(&self) -> Result<LedgerRoot, HeapFault> {
        Ok(LedgerRoot {
            owners: keyed(&self.state.owners)?,
            execution: keyed(&self.state.execution)?,
            ended: keyed(&self.state.ended)?,
            entities: keyed(&self.state.entities)?,
            refinements: self.state.refinements,
            next_invocation: self.state.next_invocation,
        })
    }
    pub(super) fn seal_with(
        &mut self,
        access: &Access<'_>,
        work: &mut SealWork,
    ) -> Result<Option<LedgerRoot>, HeapFault> {
        // A seal ends the active borrowed execution view even if its budget
        // yields. Outstanding admissions cannot survive the handoff.
        self.handoff()?;
        if self.state.execution.live_members(access)? != 0 {
            return Err(invalid("transient source owners at seal"));
        }
        self.drop_provisional(access)?;
        let mut state = self.state.clone();
        if !state.owners.seal(access, work)?
            || !state.execution.seal(access, work)?
            || !state.ended.seal(access, work)?
            || !state.entities.seal(access, work)?
        {
            self.state = state;
            return Ok(None);
        }
        let root = LedgerRoot {
            owners: keyed(&state.owners)?,
            execution: keyed(&state.execution)?,
            ended: keyed(&state.ended)?,
            entities: keyed(&state.entities)?,
            refinements: state.refinements,
            next_invocation: state.next_invocation,
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
                self.state.entities.update(key, self.access, |record| {
                    record.incoming = record
                        .incoming
                        .checked_add(edge.count)
                        .ok_or_else(|| invalid("incoming ownership overflow"))?;
                    Ok(true)
                })?;
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
                if allow_swept && self.state.entities.get(&key, self.access)?.is_none() {
                    continue;
                }
                let mut last = false;
                self.state.entities.update(key, self.access, |record| {
                    record.incoming = record
                        .incoming
                        .checked_sub(edge.count)
                        .ok_or_else(|| invalid("incoming ownership underflow"))?;
                    last = record.incoming == 0;
                    Ok(true)
                })?;
                if last {
                    self.zero.push_back(entity);
                }
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
    fn roots(&mut self, mut changes: Vec<(Owner, Option<Holding>)>) -> Result<(), HeapFault> {
        changes.sort_unstable_by_key(|(owner, _)| *owner);
        if changes.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(invalid("owner replaced twice in one transfer"));
        }
        let access = self.access;
        let mut edges = BTreeMap::<EntityId, (u64, u64)>::new();
        let mut collect = |value: &Holding, adding: bool| {
            value.each_edge(access, &mut |edge| {
                if let Identity::Object(entity) = edge.target {
                    let counts = edges.entry(entity).or_default();
                    let count = if adding { &mut counts.1 } else { &mut counts.0 };
                    *count = count
                        .checked_add(edge.count)
                        .ok_or_else(|| invalid("incoming ownership overflow"))?;
                }
                Ok(())
            })
        };
        for (owner, value) in changes {
            let key = owner_key(owner);
            let next = value.map(|value| RootHolding { owner, value });
            self.state
                .roots_mut(owner)
                .replace_with(&key, access, |old| {
                    if next.is_none() && old.is_none() {
                        return Err(invalid("released source owner is absent"));
                    }
                    if let Some(next) = &next {
                        codec::validate_root(next)?;
                        if matches!(
                            owner,
                            Owner::Value {
                                slot: Slot::Capture(_),
                                ..
                            }
                        ) {
                            next.value.each_edge(access, &mut |edge| {
                                if edge.count != 1 || !matches!(edge.target, Identity::Object(_)) {
                                    return Err(invalid("capture must borrow one variable cell"));
                                }
                                Ok(())
                            })?;
                        }
                        collect(&next.value, true)?;
                    }
                    if let Some(old) = old {
                        collect(&old.value, false)?;
                    }
                    Ok(match next {
                        Some(next)
                            if old.is_some_and(|previous| {
                                next.value.bytes == previous.value.bytes
                                    && next.value.links.is_empty()
                                    && previous.value.links.is_empty()
                            }) =>
                        {
                            index::RowEdit::Keep
                        }
                        Some(next) => index::RowEdit::Put(next),
                        None => index::RowEdit::Remove,
                    })
                })?;
        }
        for (entity, (removed, added)) in edges {
            let key = entity_key(entity);
            let next = |incoming: u64| {
                incoming
                    .checked_add(added)
                    .ok_or_else(|| invalid("incoming ownership overflow"))?
                    .checked_sub(removed)
                    .ok_or_else(|| invalid("incoming ownership underflow"))
            };
            let incoming = if removed == added {
                self.state.entities.get_with(&key, self.access, |record| {
                    next(
                        record
                            .ok_or_else(|| invalid("holding reaches absent entity"))?
                            .incoming,
                    )
                })??
            } else {
                let mut incoming = 0;
                self.state.entities.update(key, self.access, |record| {
                    incoming = next(record.incoming)?;
                    record.incoming = incoming;
                    Ok(true)
                })?;
                incoming
            };
            if incoming == 0 {
                self.zero.push_back(entity);
            }
        }
        self.release_zero(false)
    }
}

fn invalid(reason: &str) -> HeapFault {
    HeapFault::Malformed(reason.into())
}
fn entity_of_key(key: &[u8]) -> Option<EntityId> {
    let key: &[u8; 16] = key.try_into().ok()?;
    Some(EntityId {
        family: FamilyId(u64::from_be_bytes(key[..8].try_into().ok()?)),
        ordinal: u64::from_be_bytes(key[8..].try_into().ok()?),
    })
}
fn provisional(identity: Identity) -> bool {
    matches!(identity, Identity::Object(entity) if entity.family == FamilyId::PROVISIONAL)
}
fn entity_key(entity: EntityId) -> [u8; 16] {
    let mut key = [0; 16];
    key[..8].copy_from_slice(&entity.family.0.to_be_bytes());
    key[8..].copy_from_slice(&entity.ordinal.to_be_bytes());
    key
}
fn identity_key(identity: Identity) -> codec::Key {
    match identity {
        Identity::Object(entity) => codec::Key::tagged(0, &entity_key(entity)),
        Identity::Task(task) => codec::Key::tagged(1, &task.0.to_be_bytes()),
    }
}
fn owner_key(owner: Owner) -> codec::Key {
    codec::owner_key(owner)
}
fn keyed<T: Cell>(tree: &Tree<T>) -> Result<KeyedRoot, HeapFault> {
    tree.keyed_root()
}
