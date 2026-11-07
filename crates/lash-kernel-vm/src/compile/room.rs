//! Source memory programs: the obligations a region's source facts name.
//! The region ISA prices them; the source evaluator runs only as the testing
//! oracle the verified region is compared against.

use lash_kernel_doc::{self as doc, LogicalHeldV1 as Cost};
use lash_kernel_value::sites::{Site, Unit};
use lash_kernel_value::{FunctionId, Name};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

pub(crate) mod charges;
mod derive;
mod evaluate;
mod interval;
pub(crate) mod placement;
pub(crate) mod publication;
pub(crate) mod region;
pub(crate) mod root_owners;
pub(crate) use interval::CompiledInterval;
pub(crate) type Selection = interval::Selection;
mod verify;

#[derive(Clone, Copy)]
pub(crate) enum TryScope {
    Body,
    Catch,
    Finally,
}

/// What a `for … via` walks once its `open` has answered (`K-ITER-005`):
/// a list, a tuple, or a map or a set it does not check, by its own cursor;
/// a text by code point; a map or a set its protocol checks; or a driver
/// its `next` steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Walk {
    Collection,
    Guarded,
    Driver,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Phase {
    Enter,
    Open,
    Next,
    Check,
    Item,
    Test,
    Catch,
    Suspend,
    Restore,
    Supersede,
    Commit,
    Fallthrough,
    Leave,
    /// A `return via`'s policy call, after every finally of its frame.
    Policy,
}
impl Phase {
    /// Every phase, in declaration order: a phase's position is its tag in
    /// a saved continuation.
    const ALL: [Phase; lash_kernel_state::paged::ContinuationPhase::SOURCE_PHASES as usize] = [
        Phase::Enter,
        Phase::Open,
        Phase::Next,
        Phase::Check,
        Phase::Item,
        Phase::Test,
        Phase::Catch,
        Phase::Suspend,
        Phase::Restore,
        Phase::Supersede,
        Phase::Commit,
        Phase::Fallthrough,
        Phase::Leave,
        Phase::Policy,
    ];
    /// The phase's tag in a saved continuation.
    pub(crate) fn code(self) -> u32 {
        self as u32
    }
    /// The phase a saved continuation's tag names.
    pub(crate) fn from_code(code: u32) -> Option<Self> {
        Self::ALL
            .get(code as usize)
            .copied()
            .filter(|phase| phase.code() == code)
    }
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct Point {
    pub(crate) site: Site,
    pub(crate) phase: Phase,
}

/// Source owner selectors, instantiated per invocation and loop occurrence.
/// Scope selects active bindings in its source subtree; Operands selects its
/// pending source occurrences. Sessions selects durable session roots. These
/// groups are disjoint, and Frame names only the header. Departures selects
/// active and suspended departures outside scopes and operands. Suspend and
/// Restore transfer the active departure to/from the exact try's pending owner;
/// Supersede releases only that pending owner. An action preserves both.
/// Incoming names the task-owned result arriving at an entered call. Restore
/// transfers it and the call's saved source operands without new holdings.
/// No selector refers to physical registers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum Owner {
    Operand(Site),
    Selected(Site),
    Sessions(Site),
    SessionCell { at: Site, name: Name },
    Atom { at: Site, index: u32 },
    PendingDeparture(Site),
    ActiveDeparture(Site),
    LoopCursor(Site),
    LoopHeader(Site),
    Departures(Site),
    Cell { declared: Site, name: Name },
    Binding { declared: Site, name: Name },
    Visible { at: Site, name: Name },
    Scope(Site),
    Operands(Site),
    Frame(Site),
    Entity(Site),
    Incoming(Site),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CallSource {
    Body(FunctionId),
    Native(FunctionId),
    Dynamic,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Measure {
    Local(Owner),
    /// Direct member occurrences, plus record field-name payload bytes. Pooled
    /// children cost one reference; aliased mutable bodies are owned separately.
    Members(Site),
    Release(Box<[Owner]>),
    CaptureEdges(Site),
    MemberDelta(Site),
    MemberGrowth(Site),
    NativeScratch {
        at: Site,
        function: FunctionId,
    },
    CallPeak {
        at: Site,
        callee: CallSource,
    },
    CallDelta {
        at: Site,
        callee: CallSource,
    },
}

/// A checked integer polynomial over logical source facts. Only member updates
/// and callee deltas are signed. Missing facts and overflow fail eligibility.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Amount {
    pub(crate) fixed: i128,
    terms: Vec<(Measure, i128)>,
}
impl Amount {
    fn fixed(n: u64) -> Self {
        Self {
            fixed: n.into(),
            ..Self::default()
        }
    }
    fn term(mut self, m: Measure, coefficient: i128) -> Self {
        match self.terms.binary_search_by(|(measure, _)| measure.cmp(&m)) {
            Ok(at) => self.terms[at].1 += coefficient,
            Err(at) => self.terms.insert(at, (m, coefficient)),
        }
        self
    }
    /// What a protocol `for`'s header row holds once it writes `held`,
    /// less what it lets go of in `released`.
    pub(crate) fn header(held: Option<Owner>, released: Option<Owner>) -> Self {
        let mut amount = Self::default();
        if let Some(owner) = held {
            amount = amount.term(Measure::Local(owner), 1);
        }
        if let Some(owner) = released {
            amount = amount.term(release([owner]), -1);
        }
        amount
    }
    pub(crate) fn measures(&self) -> impl ExactSizeIterator<Item = &Measure> {
        self.terms.iter().map(|(measure, _)| measure)
    }
    fn evaluate(&self, facts: &dyn Facts) -> Option<i128> {
        self.terms.iter().try_fold(self.fixed, |sum, (m, c)| {
            sum.checked_add(Self::measure(m, facts)?.checked_mul(*c)?)
        })
    }
    fn measure(m: &Measure, facts: &dyn Facts) -> Option<i128> {
        let value = match m {
            Measure::CallPeak { at, callee } => i128::from(
                facts
                    .call(at, *callee)?
                    .for_call(*callee, facts.resolved(at))?
                    .peak_extra,
            ),
            Measure::CallDelta { at, callee } => {
                facts
                    .call(at, *callee)?
                    .for_call(*callee, facts.resolved(at))?
                    .retained_delta
            }
            Measure::MemberGrowth(at) => facts.measure(&Measure::MemberDelta(at.clone()))?.max(0),
            Measure::NativeScratch { at, function } => {
                let call = facts.call(at, CallSource::Native(*function))?;
                call.for_call(CallSource::Native(*function), None)?;
                i128::from(call.native_contract.as_ref()?.scratch_peak)
            }
            _ => facts.measure(m)?,
        };
        if value < 0 && !matches!(m, Measure::MemberDelta(_) | Measure::CallDelta { .. }) {
            return None;
        }
        Some(value)
    }
}

/// Facts belong to the source ledger at one occurrence. Release removes an
/// itemized owner group atomically, so aliases cannot release an entity twice.
/// A callee result can only be built by evaluating its checked source program
/// or the exact native contract; an unchecked peak number is not a fact.
pub(crate) trait Facts {
    /// All resident cell, callee and size/depth views used by a publication.
    /// Numeric-only projections deliberately cannot mint a capability.
    fn dependencies(&self) -> Option<&[lash_kernel_doc::arena::ArenaHandle]> {
        None
    }

    fn measure(&self, measure: &Measure) -> Option<i128>;
    fn call(&self, at: &Site, callee: CallSource) -> Option<&CheckedRoom>;
    fn raised(&self) -> bool;
    fn resolved(&self, at: &Site) -> Option<FunctionId>;
}
pub(crate) trait Execution {
    fn event(&mut self, point: &Point) -> Option<Box<dyn Facts + '_>>;
    /// Selected from the source condition/cursor, not a plan's trip count.
    fn choose(&mut self, site: &Site) -> Option<bool>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum ExitKind {
    Normal,
    Return,
    Raise,
    Action,
    Finish,
    Fail,
    Break,
    Continue,
    /// A ReturnVia departs as a Return does through each `finally` of its
    /// frame; past the outermost one the machine runs its policy.
    ReturnVia,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Event {
    pub(crate) point: Point,
    pub(crate) inputs: Box<[Owner]>,
    pub(crate) output: Option<Owner>,
    pub(crate) entities: Box<[Owner]>,
    pub(crate) peak: Amount,
    pub(crate) delta: Amount,
}
impl Event {
    /// This event as its Raise exit takes it when the raise departs toward
    /// a try: the raised value is published into the frame's active
    /// departure, not the event's output, and is measured there. An event
    /// that takes no delivered value (a native's answer, a checked call, a
    /// refused condition or cursor) raises before it runs: the machine
    /// delivers the raise as the frame's Incoming and the event lets go of
    /// its inputs, as a delivering action's Commit does.
    pub(crate) fn raised(&self, frame: &Site) -> Option<Self> {
        if !self
            .inputs
            .iter()
            .any(|owner| matches!(owner, Owner::Incoming(_)))
        {
            let delta = if self.inputs.is_empty() {
                Amount::default()
            } else {
                Amount::default().term(release(self.inputs.iter().cloned()), -1)
            };
            return Some(Self {
                point: self.point.clone(),
                inputs: self
                    .inputs
                    .iter()
                    .cloned()
                    .chain(std::iter::once(Owner::Incoming(frame.clone())))
                    .collect(),
                output: Some(Owner::ActiveDeparture(frame.clone())),
                entities: Box::new([]),
                peak: Amount::default(),
                delta,
            });
        }
        let from = self.output.clone()?;
        let to = Owner::ActiveDeparture(frame.clone());
        let rename = |owner: &Owner| {
            if *owner == from {
                to.clone()
            } else {
                owner.clone()
            }
        };
        let amount = |amount: &Amount| Amount {
            fixed: amount.fixed,
            terms: amount
                .terms
                .iter()
                .map(|(measure, coefficient)| {
                    let measure = match measure {
                        Measure::Local(owner) => Measure::Local(rename(owner)),
                        Measure::Release(owners) => {
                            Measure::Release(owners.iter().map(rename).collect())
                        }
                        other => other.clone(),
                    };
                    (measure, *coefficient)
                })
                .collect(),
        };
        Some(Self {
            point: self.point.clone(),
            inputs: self.inputs.clone(),
            output: Some(to.clone()),
            entities: self.entities.iter().map(rename).collect(),
            peak: amount(&self.peak),
            delta: amount(&self.delta),
        })
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Exit {
    pub(crate) point: Point,
    /// Canonical source prefix. The existing charge projection owns its units
    /// and maps this coordinate to the earliest unpaid region head.
    pub(crate) charge_prefix: Point,
    pub(crate) kind: ExitKind,
    pub(crate) owners: Box<[Owner]>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Scratch {
    pub(crate) at: Point,
    pub(crate) function: FunctionId,
    pub(crate) amount: Amount,
    /// The primitive's contract owns all scratch, qualified by function + at.
    pub(crate) normal: Point,
    pub(crate) raised: Point,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceRoom {
    pub(crate) source: Site,
    pub(crate) events: BTreeMap<Point, Event>,
    pub(crate) scratch: Box<[Scratch]>,
    pub(crate) exits: Box<[Exit]>,
}

/// Immutable source authority retained separately from the projections. The
/// verifier reads this tree, not a second consumer of SourceRoom's formulas.
#[derive(PartialEq, Eq)]
pub(super) struct BindingSlot {
    pub(super) declared: Site,
    pub(super) name: Name,
    pub(super) shared: bool,
    pub(super) index: u32,
    pub(super) borrowed: bool,
}

pub(crate) struct Source {
    bindings: Box<[BindingSlot]>,
    pub(crate) owner: doc::constant_pool::PoolOwner,
    pub(crate) site: Site,
    description: OnceLock<Box<Description>>,
    seed: Option<Seed>,
    verified: OnceLock<Result<(), Refused>>,
    owners: OnceLock<Result<Arc<OwnerSlots>, Refused>>,
    intervals: std::sync::Mutex<BTreeMap<interval::Key, Arc<CompiledInterval>>>,
    departures: std::sync::Mutex<BTreeMap<Point, Arc<publication::DepartureExit>>>,
    events: std::sync::Mutex<interval::Events>,
    leaves: std::sync::Mutex<interval::Leaves>,
    first_events: std::sync::Mutex<BTreeMap<Site, Point>>,
    next_commits: std::sync::Mutex<BTreeMap<Site, Point>>,
}

/// Source selectors receive stable indices within this exact immutable body.
/// No layout, physical slot or execution history contributes to their identity.
#[derive(Debug)]
pub(crate) struct OwnerSlots {
    pub(crate) owner: doc::constant_pool::PoolOwner,
    pub(crate) source: Site,
    /// Every concrete owner, with its role identity (the digest of its full
    /// discriminator) where a frame slot can name it.
    slots: BTreeMap<Owner, Option<lash_kernel_state::kernel_internals::logical::RoleId>>,
    pub(crate) scopes: BTreeMap<Owner, Site>,
    aliases: BTreeMap<Owner, Owner>,
    /// Each binding's role identity and whether the frame borrows it.
    binding_slots: BTreeMap<Owner, (lash_kernel_state::kernel_internals::logical::RoleId, bool)>,
    /// The per-code role table (Q4): every slot this body names, with the
    /// source role it names. Built injective; a checkpoint persists the role
    /// and restore maps it back through this table.
    roles: BTreeMap<
        lash_kernel_state::kernel_internals::logical::Slot,
        lash_kernel_state::kernel_internals::logical::SourceRole,
    >,
    groups: std::sync::Mutex<
        std::collections::HashMap<
            Owner,
            Box<[lash_kernel_state::kernel_internals::logical::Owner]>,
        >,
    >,
}
/// Equal source identities and selectors resolve equal groups; the group
/// memo is derived and does not take part.
impl PartialEq for OwnerSlots {
    fn eq(&self, other: &Self) -> bool {
        self.owner == other.owner
            && self.source == other.source
            && self.slots == other.slots
            && self.scopes == other.scopes
            && self.aliases == other.aliases
            && self.binding_slots == other.binding_slots
    }
}
impl Eq for OwnerSlots {}
impl OwnerSlots {
    pub(crate) fn get(
        &self,
        owner: &Owner,
    ) -> Option<lash_kernel_state::kernel_internals::logical::RoleId> {
        self.slots.get(owner).copied().flatten()
    }
    pub(crate) fn contains(&self, owner: &Owner) -> bool {
        self.slots.contains_key(owner)
    }
    /// The source role a slot of this body names.
    pub(crate) fn role(
        &self,
        slot: lash_kernel_state::kernel_internals::logical::Slot,
    ) -> Option<lash_kernel_state::kernel_internals::logical::SourceRole> {
        use lash_kernel_state::kernel_internals::logical::SourceRole;
        self.roles
            .get(&slot)
            .cloned()
            .or_else(|| SourceRole::positional_of(slot))
    }
}

/// One immutable compilation unit shared by its lexical code bodies. The
/// registry is the exact snapshot compilation resolved against. No executable
/// or prepared library is retained, so the description cannot retain itself.
struct Seed {
    unit: UnitBody,
    params: Box<[Name]>,
    catalog: Arc<doc::FunctionRegistry>,
    private: std::collections::BTreeSet<Name>,
}

/// Registered bodies already belong to the immutable prepared registry.
/// Inline units retain one AST shared by all their lexical code descriptions.
#[derive(Clone)]
pub(super) enum UnitBody {
    Inline(Arc<doc::Block>),
    Registered(FunctionId),
}
impl UnitBody {
    pub(super) fn capture(unit: &Unit, body: &doc::Block, catalog: &doc::FunctionRegistry) -> Self {
        if let Unit::Library(function) = unit
            && catalog
                .get(function)
                .is_some_and(|r| r.definition().body().is_some())
        {
            Self::Registered(*function)
        } else {
            Self::Inline(Arc::new(body.clone()))
        }
    }
    fn block<'a>(&'a self, catalog: &'a doc::FunctionRegistry) -> &'a doc::Block {
        match self {
            Self::Inline(body) => body,
            Self::Registered(function) => {
                &catalog
                    .get(function)
                    .and_then(|r| r.definition().body())
                    .unwrap_or_else(|| unreachable!("exact registered source body"))
                    .block
            }
        }
    }
}

/// Cold field access keeps the SourceRoom API while initializing its source
/// authority once. Ordinary compilation uses only Source's owner and site.
pub(crate) struct Description {
    pub(crate) owner: doc::constant_pool::PoolOwner,
    pub(crate) site: Site,
    pub(crate) body: Arc<doc::Block>,
    pub(crate) params: Box<[Name]>,
    pub(crate) shared: BTreeMap<(Site, Name), bool>,
    pub(crate) persistent: BTreeMap<Site, Name>,
    cell_assignments: Result<BTreeMap<Site, Name>, Refused>,
    pub(crate) calls: BTreeMap<FunctionId, CallSource>,
    pub(crate) room: SourceRoom,
    // Preserve the first exit and alternatives in their source-room order.
    exit_kinds: BTreeMap<Point, Vec<ExitKind>>,
}
impl Description {
    fn exits_at(&self, at: &Point) -> &[ExitKind] {
        self.exit_kinds.get(at).map_or(&[], Vec::as_slice)
    }

    pub(crate) fn assigns_cell(&self, at: &Site) -> bool {
        self.cell_assignments
            .as_ref()
            .is_ok_and(|assignments| assignments.contains_key(at))
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    Source,
    Peak,
    Retained,
    Scratch,
    Exit,
    Path,
    Facts,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Balance {
    pub(crate) peak_extra: u64,
    pub(crate) retained_delta: i128,
}

/// Constructors are private to checked source/native evaluation.
pub(crate) struct CheckedRoom {
    owner: doc::constant_pool::PoolOwner,
    native: bool,
    whole: bool,
    native_contract: Option<CheckedNative>,
    balance: Balance,
    exit: ExitKind,
}
struct CheckedNative {
    contract: doc::NativeResourceContract,
    scratch_peak: u64,
}
impl CheckedRoom {
    fn for_call(&self, callee: CallSource, resolved: Option<FunctionId>) -> Option<&Balance> {
        let function = match callee {
            CallSource::Body(f) | CallSource::Native(f) => f,
            CallSource::Dynamic => resolved?,
        };
        (self.whole
            && self.owner == doc::constant_pool::PoolOwner::Function(function)
            && (matches!(callee, CallSource::Dynamic)
                || self.native == matches!(callee, CallSource::Native(_)))
            && matches!(
                self.exit,
                ExitKind::Normal | ExitKind::Return | ExitKind::Raise
            ))
        .then_some(&self.balance)
    }
    /// Input holdings remain with the caller until the primitive exits. This
    /// bound includes temporary entities that leave before the returned exit.
    pub(crate) fn native(
        function: FunctionId,
        contract: &doc::NativeResourceContract,
    ) -> Option<Self> {
        let (retained, peak, scratch_peak) = contract.balances()?;
        Some(Self {
            owner: doc::constant_pool::PoolOwner::Function(function),
            native: true,
            whole: true,
            native_contract: Some(CheckedNative {
                contract: contract.clone(),
                scratch_peak,
            }),
            balance: Balance {
                peak_extra: peak,
                retained_delta: retained.into(),
            },
            exit: match contract.exit {
                doc::NativeResourceExit::Returned => ExitKind::Return,
                doc::NativeResourceExit::Raised => ExitKind::Raise,
            },
        })
    }
    pub(crate) fn balance(&self) -> Balance {
        self.balance
    }
}
impl std::ops::Deref for Source {
    type Target = Description;
    fn deref(&self) -> &Description {
        self.description.get_or_init(|| {
            let seed = self
                .seed
                .as_ref()
                .unwrap_or_else(|| unreachable!("source seed"));
            let mut node = doc::Node::Block(seed.unit.block(&seed.catalog));
            for index in &self.site.path {
                node = *node
                    .children()
                    .get(*index as usize)
                    .unwrap_or_else(|| unreachable!("canonical source body"));
            }
            let doc::Node::Block(body) = node else {
                unreachable!("a code site names its source block")
            };
            let body = match (&seed.unit, self.site.path.is_empty()) {
                (UnitBody::Inline(unit), true) => Arc::clone(unit),
                _ => Arc::new(body.to_vec()),
            };
            Box::new(Self::form(
                self.owner,
                self.site.clone(),
                body,
                &seed.params,
                self.bindings
                    .iter()
                    .map(|binding| {
                        (
                            (binding.declared.clone(), binding.name.clone()),
                            binding.shared,
                        )
                    })
                    .collect(),
                seed.catalog.as_ref(),
                &seed.private,
            ))
        })
    }
}
impl Source {
    pub(super) fn same_bindings(&self, other: &Self) -> bool {
        self.owner == other.owner && self.site == other.site && self.bindings == other.bindings
    }

    pub(super) fn closure_free(
        &self,
        function: &FunctionId,
    ) -> Result<std::collections::BTreeSet<Name>, Refused> {
        let catalog = &self.seed.as_ref().ok_or(Refused::Source)?.catalog;
        let definition = catalog.get(function).ok_or(Refused::Source)?.definition();
        let params: Vec<_> = definition
            .signature
            .params
            .iter()
            .map(|p| p.name.clone())
            .collect();
        let body = definition.body().ok_or(Refused::Source)?;
        Ok(doc::snapshot::CodeReads::of(&params, &body.block, &**catalog).free)
    }

    pub(crate) fn binding_root(
        &self,
        index: u32,
        task: doc::TaskId,
        invocation: u64,
    ) -> Option<lash_kernel_state::kernel_internals::logical::Owner> {
        use lash_kernel_state::kernel_internals::logical::{Owner as Root, Slot, SourceRole};
        let binding = self.bindings.get(index as usize)?;
        let id = SourceRole::Binding {
            declared: binding.declared.clone(),
            name: binding.name.clone(),
        }
        .id();
        (binding.index == index).then_some(Root::Value {
            task,
            invocation,
            slot: if binding.borrowed {
                Slot::Capture(id)
            } else {
                Slot::Binding(id)
            },
        })
    }

    pub(super) fn deferred(
        owner: doc::constant_pool::PoolOwner,
        site: Site,
        unit: UnitBody,
        params: &[Name],
        bindings: Vec<BindingSlot>,
        catalog: Arc<doc::FunctionRegistry>,
        private: &std::collections::BTreeSet<Name>,
    ) -> Self {
        Self {
            owner,
            site,
            bindings: bindings.into(),
            description: OnceLock::new(),
            verified: OnceLock::new(),
            owners: OnceLock::new(),
            intervals: Default::default(),
            departures: Default::default(),
            events: Default::default(),
            leaves: Default::default(),
            first_events: Default::default(),
            next_commits: Default::default(),
            seed: Some(Seed {
                unit,
                params: params.into(),
                catalog,
                private: private.clone(),
            }),
        }
    }
    #[cfg(test)]
    pub(crate) fn compile(
        owner: doc::constant_pool::PoolOwner,
        site: Site,
        body: &doc::Block,
        params: &[Name],
        shared: BTreeMap<(Site, Name), bool>,
        catalog: &dyn doc::FunctionCatalog,
        private: &std::collections::BTreeSet<Name>,
    ) -> Self {
        let description = Self::form(
            owner,
            site.clone(),
            Arc::new(body.clone()),
            params,
            shared,
            catalog,
            private,
        );
        Self {
            owner,
            site,
            bindings: Box::new([]),
            description: OnceLock::from(Box::new(description)),
            seed: None,
            verified: OnceLock::new(),
            owners: OnceLock::new(),
            intervals: Default::default(),
            departures: Default::default(),
            events: Default::default(),
            leaves: Default::default(),
            first_events: Default::default(),
            next_commits: Default::default(),
        }
    }
    fn form(
        owner: doc::constant_pool::PoolOwner,
        site: Site,
        source_body: Arc<doc::Block>,
        params: &[Name],
        mut shared: BTreeMap<(Site, Name), bool>,
        catalog: &dyn doc::FunctionCatalog,
        private: &std::collections::BTreeSet<Name>,
    ) -> Description {
        let body = source_body.as_ref();
        let mut persistent = BTreeMap::new();
        if site.unit == Unit::Main {
            for (index, stmt) in body.iter().enumerate() {
                if let doc::Stmt::Let { name, .. } = stmt
                    && !private.contains(name)
                {
                    let at = child(&site, index);
                    persistent.insert(at.clone(), name.clone());
                    // A persistent name owns a session value, without a variable cell.
                    shared.insert((at, name.clone()), false);
                }
            }
        }
        let mut calls = BTreeMap::new();
        let mut pending = vec![doc::Node::Block(body)];
        while let Some(node) = pending.pop() {
            let function = match node {
                doc::Node::Expr(doc::Expr::Call { function, .. }) => Some(function),
                doc::Node::Expr(doc::Expr::Act(act)) => act_local_callee(act),
                _ => None,
            };
            if let Some(function) = function {
                let body = catalog.header(function).is_some_and(|h| {
                    matches!(
                        h.implementation,
                        doc::ImplementationKind::Guest | doc::ImplementationKind::Both
                    )
                });
                calls.insert(
                    *function,
                    if body {
                        CallSource::Body(*function)
                    } else {
                        CallSource::Native(*function)
                    },
                );
            }
            // A protocol `for`'s hooks and a `return via`'s policy are calls.
            let targets: Vec<&doc::FunctionTarget> = match node {
                doc::Node::Stmt(doc::Stmt::For { via: Some(via), .. }) => [&via.open]
                    .into_iter()
                    .chain(via.next.iter())
                    .chain(via.check.iter())
                    .chain(via.item.iter())
                    .collect(),
                doc::Node::Stmt(doc::Stmt::ReturnVia { policy, .. }) => vec![policy],
                _ => Vec::new(),
            };
            {
                for target in targets {
                    if let doc::FunctionTarget::Local(function) = target {
                        let has_body = catalog.header(function).is_some_and(|h| {
                            matches!(
                                h.implementation,
                                doc::ImplementationKind::Guest | doc::ImplementationKind::Both
                            )
                        });
                        calls.insert(
                            *function,
                            if has_body {
                                CallSource::Body(*function)
                            } else {
                                CallSource::Native(*function)
                            },
                        );
                    }
                }
            }
            node.children_into(&mut pending);
        }
        let mut source = Description {
            owner,
            site: site.clone(),
            body: Arc::clone(&source_body),
            params: params.into(),
            shared,
            persistent,
            cell_assignments: Ok(BTreeMap::new()),
            calls,
            room: SourceRoom {
                source: site,
                events: BTreeMap::new(),
                scratch: Box::new([]),
                exits: Box::new([]),
            },
            exit_kinds: BTreeMap::new(),
        };
        source.cell_assignments = root_owners::cell_assignments(&source);
        source.room = derive::body(&source);
        for exit in &source.room.exits {
            source
                .exit_kinds
                .entry(exit.point.clone())
                .or_default()
                .push(exit.kind);
        }
        source
    }
    pub(crate) fn verify(&self) -> Result<(), Refused> {
        self.verify_identity()?;
        *self.verified.get_or_init(|| verify::body(self, &self.room))
    }
    /// Active ownership consumers explicitly force the cached independent
    /// verdict. Indexing reads only the verified room, with no further AST walk.
    pub(crate) fn owner_slots(&self) -> Result<Arc<OwnerSlots>, Refused> {
        self.verify()?;
        self.owners
            .get_or_init(|| {
                let mut owners = std::collections::BTreeSet::new();
                owners.insert(Owner::ActiveDeparture(self.site.clone()));
                // A raise the machine serves arrives as the frame's Incoming.
                owners.insert(Owner::Incoming(self.site.clone()));
                for event in self.room.events.values() {
                    if event.point.phase == Phase::Test && matches!(&event.output, Some(Owner::Binding { declared, .. }) if *declared == child(&event.point.site, 1)) {
                        owners.insert(Owner::LoopCursor(event.point.site.clone()));
                        owners.insert(Owner::LoopHeader(event.point.site.clone()));
                        // A protocol's hooks return into the loop's Incoming,
                        // which its header rows take and let go of.
                        if self.room.events.contains_key(&Point {
                            site: event.point.site.clone(),
                            phase: Phase::Open,
                        }) {
                            owners.insert(Owner::Incoming(event.point.site.clone()));
                        }
                    }
                    owners.extend(event.inputs.iter().cloned());
                    owners.extend(event.output.iter().cloned());
                    owners.extend(event.entities.iter().cloned());
                    for amount in [&event.peak, &event.delta] {
                        for measure in amount.measures() {
                            match measure {
                                Measure::Local(owner) => {
                                    owners.insert(owner.clone());
                                }
                                Measure::Release(group) => {
                                    owners.extend(group.iter().cloned());
                                }
                                _ => {}
                            }
                        }
                    }
                }
                for exit in &self.room.exits {
                    owners.extend(exit.owners.iter().cloned());
                }
                for binding in &self.bindings {
                    owners.insert(Owner::Binding {
                        declared: binding.declared.clone(),
                        name: binding.name.clone(),
                    });
                }
                for name in &self.params {
                    owners.insert(Owner::Binding {
                        declared: self.site.clone(),
                        name: name.clone(),
                    });
                }
                for (at, name) in self.cell_assignments.as_ref().map_err(|error| *error)? {
                    owners.insert(Owner::Visible {
                        at: at.clone(),
                        name: name.clone(),
                    });
                }
                for ((declared, name), shared) in &self.shared {
                    if *shared && declared == &self.site {
                        owners.insert(Owner::Binding {
                            declared: declared.clone(),
                            name: name.clone(),
                        });
                    }
                }
                let (scopes, aliases, binding_slots) = root_owners::form(self, &owners)?;
                let slots = owners
                    .into_iter()
                    .map(|owner| {
                        let id = root_owners::source_role(&owner).map(|role| role.id());
                        (owner, id)
                    })
                    .collect::<BTreeMap<_, _>>();
                let mut table = OwnerSlots {
                    owner: self.owner,
                    source: self.site.clone(),
                    slots,
                    scopes,
                    aliases,
                    binding_slots,
                    roles: BTreeMap::new(),
                    groups: Default::default(),
                };
                table.roles = table.role_table()?;
                Ok(Arc::new(table))
            })
            .clone()
    }
    fn verify_identity(&self) -> Result<(), Refused> {
        let description = std::ops::Deref::deref(self);
        if self.owner != description.owner
            || self.site != description.site
            || description.cell_assignments.is_err()
        {
            return Err(Refused::Source);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn verify_room(&self, room: &SourceRoom) -> Result<(), Refused> {
        self.verify_identity()?;
        // A changed projection is never covered by the immutable default's cache.
        verify::body(self, room)
    }
    /// The first canonical holding event of an actual source continuation.
    /// This uses the same walker as interval verification and stops before
    /// choosing a branch or evaluating a fact.
    pub(crate) fn first_event(&self, at: &Site) -> Result<Point, Refused> {
        self.verify()?;
        let mut cache = self.first_events.lock().map_err(|_| Refused::Source)?;
        if let Some(point) = cache.get(at) {
            return Ok(point.clone());
        }
        struct First(Option<Point>);
        impl evaluate::Events for First {
            fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
                self.0 = Some(point(site, phase));
                Err(Refused::Path)
            }
            fn choose(&mut self, _: &Site) -> Result<bool, Refused> {
                Err(Refused::Facts)
            }
        }
        let mut first = First(None);
        let _ = evaluate::walk(self, at, &mut first, 4096);
        let point = first.0.ok_or(Refused::Path)?;
        cache.insert(at.clone(), point.clone());
        Ok(point)
    }

    /// The actual selected continuation of a while condition. Both endpoints
    /// come from the verified source tree, before any loop-control mutation.
    pub(crate) fn next_after_while_test(&self, at: &Site, taken: bool) -> Result<Point, Refused> {
        self.verify()?;
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return Err(Refused::Path);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        let doc::Node::Stmt(doc::Stmt::While { else_block, .. }) = node else {
            return Err(Refused::Path);
        };
        if taken {
            self.first_event(&child(at, 1))
        } else if !else_block.is_empty() {
            self.first_event(&child(at, 2))
        } else {
            self.room
                .events
                .get(&point(at, Phase::Commit))
                .map(|event| event.point.clone())
                .ok_or(Refused::Path)
        }
    }

    /// The selected arm is an actual child of this source conditional.
    /// Its first event is determined before either operand or control changes.
    pub(crate) fn next_after_if_test(&self, at: &Site, taken: bool) -> Result<Point, Refused> {
        self.verify()?;
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return Err(Refused::Path);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        if !matches!(
            node,
            doc::Node::Expr(doc::Expr::If { .. }) | doc::Node::Stmt(doc::Stmt::If { .. })
        ) {
            return Err(Refused::Path);
        }
        self.first_event(&child(at, if taken { 1 } else { 2 }))
    }

    /// The next event after a resident, non-protocol cursor has selected its branch.
    pub(crate) fn next_after_for_cursor(
        &self,
        at: &Site,
        continued: bool,
    ) -> Result<Point, Refused> {
        self.verify()?;
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return Err(Refused::Path);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        let doc::Node::Stmt(doc::Stmt::For {
            via: None,
            else_block,
            ..
        }) = node
        else {
            return Err(Refused::Path);
        };
        if !continued && !else_block.is_empty() {
            return self.first_event(&child(at, 2));
        }
        self.room
            .events
            .get(&point(
                at,
                if continued {
                    Phase::Test
                } else {
                    Phase::Commit
                },
            ))
            .map(|event| event.point.clone())
            .ok_or(Refused::Path)
    }

    /// A normally completed try block reaches its actual cleanup or Try Commit.
    /// An exceptional departure has a different source recipe.
    pub(crate) fn next_after_try_scope(
        &self,
        at: &Site,
        scope: TryScope,
    ) -> Result<Point, Refused> {
        self.verify()?;
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return Err(Refused::Path);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        let doc::Node::Stmt(doc::Stmt::Try(statement)) = node else {
            return Err(Refused::Path);
        };
        if matches!(scope, TryScope::Catch) && statement.catch.is_none() {
            return Err(Refused::Path);
        }
        if matches!(scope, TryScope::Finally) && statement.finally.is_none() {
            return Err(Refused::Path);
        }
        if !matches!(scope, TryScope::Finally) && statement.finally.is_some() {
            self.first_event(&at.child(if statement.catch.is_some() { 2 } else { 1 }))
        } else {
            let next = point(at, Phase::Commit);
            self.room
                .events
                .contains_key(&next)
                .then_some(next)
                .ok_or(Refused::Path)
        }
    }

    /// The source node at `at`, a site of this source.
    pub(crate) fn node_at(&self, at: &Site) -> Option<doc::Node<'_>> {
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return None;
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize)?;
        }
        Some(node)
    }
    /// Whether the source node is a native call act that answers at its Enter.
    pub(crate) fn answers_at_enter(&self, at: &Site) -> bool {
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return false;
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.path[self.site.path.len()..] {
            let Some(child) = node.children().get(*index as usize).copied() else {
                return false;
            };
            node = child;
        }
        native_act(node, &self.calls).is_some()
    }
    pub(crate) fn next_after_commit(&self, at: &Site) -> Result<Point, Refused> {
        self.verify()?;
        if at.unit != self.site.unit
            || !at.path.starts_with(&self.site.path)
            || at.path.len() <= self.site.path.len()
        {
            return Err(Refused::Path);
        }
        let mut cache = self.next_commits.lock().map_err(|_| Refused::Source)?;
        if let Some(next) = cache.get(at) {
            return Ok(next.clone());
        }
        let mut node = doc::Node::Block(&self.body);
        let mut parent = node;
        for index in &at.path[self.site.path.len()..] {
            parent = node;
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        if !matches!(
            node,
            doc::Node::Expr(_)
                | doc::Node::Action(_)
                | doc::Node::Stmt(
                    doc::Stmt::Let { .. }
                        | doc::Stmt::Do { .. }
                        | doc::Stmt::Assign { .. }
                        | doc::Stmt::Remove { .. }
                        | doc::Stmt::Print { .. }
                )
        ) || !self.room.events.contains_key(&point(at, Phase::Commit))
        {
            return Err(Refused::Path);
        }
        let mut enclosing = at.clone();
        let index = enclosing.path.pop().ok_or(Refused::Path)?;
        struct Next<'a> {
            parent: &'a Site,
            completed: &'a Site,
            selected: Option<bool>,
            passed: bool,
            next: Option<Point>,
        }
        impl evaluate::Events for Next<'_> {
            fn completed(&mut self, site: &Site) -> Option<ExitKind> {
                if self.passed || site == self.parent {
                    return None;
                }
                if site == self.completed {
                    self.passed = true;
                }
                Some(ExitKind::Normal)
            }
            fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
                if self.passed {
                    self.next = Some(point(site, phase));
                    Err(Refused::Path)
                } else {
                    Ok(ExitKind::Normal)
                }
            }
            fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
                if !self.passed && site == self.parent {
                    self.selected.ok_or(Refused::Facts)
                } else {
                    Err(Refused::Facts)
                }
            }
        }
        let selected = match parent {
            doc::Node::Expr(doc::Expr::If { .. }) if index == 1 || index == 2 => Some(index == 1),
            _ => None,
        };
        let mut next = Next {
            parent: &enclosing,
            completed: at,
            selected,
            passed: false,
            next: None,
        };
        let _ = evaluate::walk(self, &enclosing, &mut next, 32);
        let successor = next.next.ok_or(Refused::Facts)?;
        if !next.passed || !self.room.events.contains_key(&successor) {
            return Err(Refused::Path);
        }
        cache.insert(at.clone(), successor.clone());
        Ok(successor)
    }

    #[cfg(test)]
    /// A finite source execution, including selected branches and repeated
    /// source loop tests. Budget exhaustion is an eligibility miss.
    pub(crate) fn evaluate(
        &self,
        execution: &mut impl Execution,
        budget: usize,
    ) -> Result<CheckedRoom, Refused> {
        self.verify()?;
        evaluate::prefix(self, &self.site, execution, budget)
    }
    #[cfg(test)]
    pub(crate) fn evaluate_prefix(
        &self,
        at: &Site,
        execution: &mut impl Execution,
        budget: usize,
    ) -> Result<CheckedRoom, Refused> {
        self.verify()?;
        evaluate::prefix(self, at, execution, budget)
    }
}
fn point(site: &Site, phase: Phase) -> Point {
    Point {
        site: site.clone(),
        phase,
    }
}
fn child(site: &Site, index: usize) -> Site {
    let mut s = site.clone();
    s.path.push(index as u32);
    s
}
fn operand(site: &Site) -> Owner {
    Owner::Operand(site.clone())
}
/// The library function a call act calls natively. Such an act is a native
/// call at its Enter, which publishes the answer as its operand: no callee
/// is entered and no Incoming is delivered.
pub(crate) fn native_act(
    node: doc::Node<'_>,
    calls: &BTreeMap<FunctionId, CallSource>,
) -> Option<FunctionId> {
    match node {
        doc::Node::Expr(doc::Expr::Act(act)) => act_local_callee(act)
            .filter(|function| matches!(calls.get(function), Some(CallSource::Native(_))))
            .copied(),
        _ => None,
    }
}
/// Whether an expression act calls a function: a call, or an await, which
/// calls its policy with the awaited value (`K-TASK-026`).
pub(crate) fn act_calls(act: &doc::ActExpr) -> bool {
    matches!(act.kind, doc::ActKind::Call(_) | doc::ActKind::Await(_))
}
/// The local function an expression act calls: a call's local callee, or
/// an await's local policy.
pub(crate) fn act_local_callee(act: &doc::ActExpr) -> Option<&FunctionId> {
    match &act.kind {
        doc::ActKind::Call(doc::Callee::Local(function))
        | doc::ActKind::Await(doc::FunctionTarget::Local(function)) => Some(function),
        _ => None,
    }
}
/// An action whose result the machine delivers as the task's Incoming:
/// every statement action, and every nested one except a call or an await,
/// whose callee completion enters a distinct Restore. Its Commit moves that
/// Incoming into the action's operand, as Restore does.
pub(crate) fn delivers(node: doc::Node<'_>) -> bool {
    match node {
        doc::Node::Action(_) => true,
        doc::Node::Expr(doc::Expr::Act(act)) => !act_calls(act),
        _ => false,
    }
}
/// The inputs of `node`'s Commit: a delivering action's also take its
/// delivered Incoming.
fn commit_inputs(site: &Site, node: doc::Node<'_>) -> Box<[Owner]> {
    let ins = inputs(site, node);
    if !delivers(node) {
        return ins;
    }
    ins.iter()
        .cloned()
        .chain(std::iter::once(Owner::Incoming(site.clone())))
        .collect()
}
fn inputs(site: &Site, node: doc::Node<'_>) -> Box<[Owner]> {
    match node {
        doc::Node::Expr(doc::Expr::Act(act))
            if matches!(
                act.kind,
                doc::ActKind::Call(doc::Callee::Value(_))
                    | doc::ActKind::Spawn(doc::Callee::Value(_))
            ) =>
        {
            return std::iter::once(operand(site))
                .chain((0..act.args.len()).map(|index| operand(&child(site, index))))
                .collect();
        }
        doc::Node::Expr(doc::Expr::If { .. }) => return vec![Owner::Selected(site.clone())].into(),
        doc::Node::Expr(doc::Expr::Let { .. }) => return vec![operand(&child(site, 1))].into(),
        doc::Node::Stmt(doc::Stmt::If { .. } | doc::Stmt::While { .. }) => return Box::new([]),
        _ => {}
    }
    node.children()
        .iter()
        .enumerate()
        .filter(|(_, n)| matches!(n, doc::Node::Expr(_) | doc::Node::Action(_)))
        .map(|(i, _)| operand(&child(site, i)))
        .collect()
}
/// Whether an event at `node`'s `phase` can raise besides taking its own
/// exit: an expression or action may raise, a protocol hook's callee may
/// raise, a plain `for` may refuse what it cannot walk, and a condition may
/// refuse a non-bool. A `return via`'s policy may raise at its call, a
/// write or removal of a member may refuse its key at its commit, and a
/// print refuses a value that is no data at its commit.
fn raises(node: doc::Node<'_>, phase: Phase) -> bool {
    matches!(node, doc::Node::Expr(_) | doc::Node::Action(_))
        || hook_raises(node, phase)
        || (matches!(
            node,
            doc::Node::Stmt(
                doc::Stmt::Assign {
                    place: doc::Place::Member(_),
                    ..
                } | doc::Stmt::Remove { .. }
                    | doc::Stmt::Print { .. }
            )
        ) && phase == Phase::Commit)
        || (matches!(node, doc::Node::Stmt(doc::Stmt::ReturnVia { .. })) && phase == Phase::Policy)
        || (matches!(node, doc::Node::Stmt(doc::Stmt::For { via: None, .. }))
            && phase == Phase::Enter)
        || (matches!(
            node,
            doc::Node::Stmt(doc::Stmt::If { .. } | doc::Stmt::While { .. })
        ) && phase == Phase::Test)
}
fn release(owners: impl IntoIterator<Item = Owner>) -> Measure {
    Measure::Release(owners.into_iter().collect())
}
fn shared(shared: &BTreeMap<(Site, Name), bool>, site: &Site, name: &Name) -> u64 {
    if shared.get(&(site.clone(), name.clone())) == Some(&true) {
        Cost::OBJECT + Cost::REFERENCE
    } else {
        0
    }
}

pub(crate) fn action_atoms(action: &doc::Action) -> &[doc::Atom] {
    match action {
        doc::Action::Call { args, .. }
        | doc::Action::Spawn { args, .. }
        | doc::Action::Perform { args, .. } => args,
        doc::Action::Join { task }
        | doc::Action::Cancel { task }
        | doc::Action::CancelEffect { task } => std::slice::from_ref(task),
        doc::Action::JoinMany { tasks, .. } => std::slice::from_ref(tasks),
        doc::Action::Await { value, .. } => std::slice::from_ref(value),
        doc::Action::Yield => &[],
    }
}

/// Whether a raise of a protocol hook's callee is born at `phase` of the
/// `for` at `node`: at the hook's call, or, for `item`, at the Test that
/// takes its result.
pub(crate) fn hook_raises(node: doc::Node<'_>, phase: Phase) -> bool {
    let doc::Node::Stmt(doc::Stmt::For { via: Some(via), .. }) = node else {
        return false;
    };
    match phase {
        Phase::Test => via.item.is_some(),
        phase => hooks(via).any(|(hook, _)| hook == phase),
    }
}

/// A protocol's declared hooks, each with the header phase it is called at.
pub(crate) fn hooks(via: &doc::Protocol) -> impl Iterator<Item = (Phase, &doc::FunctionTarget)> {
    [
        (Phase::Open, Some(&via.open)),
        (Phase::Next, via.next.as_ref()),
        (Phase::Check, via.check.as_ref()),
        (Phase::Item, via.item.as_ref()),
    ]
    .into_iter()
    .filter_map(|(phase, target)| Some((phase, target?)))
}

/// The native a protocol hook at `phase` of the `for` at `node`, or a
/// `return via`'s policy, calls, when it calls one.
pub(crate) fn native_hook(
    node: doc::Node<'_>,
    phase: Phase,
    calls: &BTreeMap<FunctionId, CallSource>,
) -> Option<FunctionId> {
    let target = match node {
        doc::Node::Stmt(doc::Stmt::For { via: Some(via), .. }) => {
            hooks(via).find_map(|(hook, target)| (hook == phase).then_some(target))?
        }
        doc::Node::Stmt(doc::Stmt::ReturnVia { policy, .. }) if phase == Phase::Policy => policy,
        _ => return None,
    };
    match target {
        doc::FunctionTarget::Local(function) => match calls.get(function)? {
            CallSource::Native(native) => Some(*native),
            _ => None,
        },
        doc::FunctionTarget::Import(_) => None,
    }
}

pub(crate) fn hook(
    target: &doc::FunctionTarget,
    calls: &BTreeMap<FunctionId, CallSource>,
) -> CallSource {
    match target {
        doc::FunctionTarget::Local(f) => calls[f],
        doc::FunctionTarget::Import(_) => CallSource::Dynamic,
    }
}
