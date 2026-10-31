//! Source memory programs. Compilation records source obligations; production
//! admission continues to use its existing metric until the ledger cutover.
#![allow(
    dead_code,
    reason = "checked phase-one projection; admission consumes it in phase two"
)]

use lash_kernel_doc::{self as doc, LogicalHeldV1 as Cost};
use lash_kernel_value::sites::{Site, Unit};
use lash_kernel_value::{FunctionId, Name};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

mod bulk;
mod derive;
mod evaluate;
mod interval;
pub(crate) mod publication;
mod root_owners;
pub(crate) use interval::CompiledInterval;
pub(crate) type Selection = interval::Selection;
mod verify;

#[derive(Clone, Copy)]
pub(crate) enum TransferTarget {
    Binding,
    RootReturn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
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
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Point {
    pub(crate) site: Site,
    pub(crate) phase: Phase,
}

pub(crate) struct Prefix<'a> {
    pub(crate) source: &'a Arc<Source>,
    pub(crate) at: &'a Site,
}

/// Source owner selectors, instantiated per invocation and loop occurrence.
/// Scope selects active bindings in its source subtree; Operands selects its
/// pending source occurrences. Sessions selects durable session roots. These
/// groups are disjoint, and Frame names only the header. Departures selects
/// active and suspended departures outside scopes and operands. Suspend and
/// Restore transfer the active departure to/from the exact try's pending owner;
/// Supersede releases only that pending owner. An action preserves both.
/// No selector refers to physical registers.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Owner {
    Operand(Site),
    Selected(Site),
    Sessions(Site),
    SessionCell { at: Site, name: Name },
    Atom { at: Site, index: u32 },
    Departure(Site),
    PendingDeparture(Site),
    ActiveDeparture(Site),
    LoopCursor(Site),
    LoopHeader(Site),
    Departures(Site),
    HookArgument { at: Site, phase: Phase, index: u32 },
    HookResult { at: Site, phase: Phase },
    Cell { declared: Site, name: Name },
    Binding { declared: Site, name: Name },
    Visible { at: Site, name: Name },
    Scope(Site),
    Operands(Site),
    Frame(Site),
    Entity(Site),
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
    NativeRelease {
        at: Site,
        callee: CallSource,
        owners: Box<[Owner]>,
    },
}

/// A checked integer polynomial over logical source facts. Only member updates
/// and callee deltas are signed. Missing facts and overflow fail eligibility.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Amount {
    pub(crate) fixed: i128,
    pub(crate) terms: BTreeMap<Measure, i128>,
}
impl Amount {
    fn fixed(n: u64) -> Self {
        Self {
            fixed: n.into(),
            ..Self::default()
        }
    }
    fn term(mut self, m: Measure, coefficient: i128) -> Self {
        *self.terms.entry(m).or_default() += coefficient;
        self
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
                i128::from(call.native_contract.as_ref()?.peak_scratch_bytes()?)
            }
            Measure::NativeRelease { at, callee, owners } => {
                let called = facts.call(at, *callee)?;
                called.for_call(*callee, facts.resolved(at))?;
                if called.native {
                    facts.measure(&Measure::Release(owners.clone()))?
                } else {
                    0
                }
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
    pub(crate) bulk: Option<bulk::Bulk>,
    pub(crate) peak_extra: BTreeMap<Point, Event>,
    pub(crate) retained_delta: BTreeMap<Point, Event>,
    pub(crate) scratch: Box<[Scratch]>,
    pub(crate) exits: Box<[Exit]>,
}

/// Immutable source authority retained separately from the projections. The
/// verifier reads this tree, not a second consumer of SourceRoom's formulas.
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
    first_events: std::sync::Mutex<BTreeMap<Site, Point>>,
    next_commits: std::sync::Mutex<BTreeMap<Site, Point>>,
}

/// Source selectors receive stable indices within this exact immutable body.
/// No layout, physical slot or execution history contributes to their identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct OwnerSlots {
    pub(crate) owner: doc::constant_pool::PoolOwner,
    pub(crate) source: Site,
    slots: BTreeMap<Owner, u32>,
    controls: Box<[Owner]>,
    scopes: BTreeMap<Owner, Site>,
    aliases: BTreeMap<Owner, Owner>,
    binding_slots: BTreeMap<Owner, (u32, bool)>,
}
impl OwnerSlots {
    pub(crate) fn get(&self, owner: &Owner) -> Option<u32> {
        self.slots.get(owner).copied()
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
    pub(super) bulk: Option<&'static doc::plan::CopyLoop>,
    pub(crate) calls: BTreeMap<FunctionId, CallSource>,
    pub(crate) room: SourceRoom,
}
impl Description {
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
    native_contract: Option<doc::NativeResourceContract>,
    balance: Balance,
    exit: ExitKind,
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
        let retained = contract.retained_bytes()?;
        let peak = contract.peak_extra_bytes()?;
        Some(Self {
            owner: doc::constant_pool::PoolOwner::Function(function),
            native: true,
            whole: true,
            native_contract: Some(contract.clone()),
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
    pub(crate) fn binding_root(
        &self,
        index: u32,
        task: doc::TaskId,
        invocation: u64,
    ) -> Option<lash_kernel_state::kernel_internals::logical::Owner> {
        use lash_kernel_state::kernel_internals::logical::{Owner as Root, Slot};
        let binding = self.bindings.get(index as usize)?;
        (binding.index == index).then_some(Root::Value {
            task,
            invocation,
            slot: if binding.borrowed {
                Slot::Capture(index)
            } else {
                Slot::Binding(index)
            },
        })
    }
    #[cfg(test)]
    pub(crate) fn is_derived(&self) -> bool {
        self.description.get().is_some()
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
            if let doc::Node::Expr(doc::Expr::Call { function, .. }) = node {
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
            if let doc::Node::Stmt(doc::Stmt::For { via: Some(via), .. }) = node {
                for target in [&via.open]
                    .into_iter()
                    .chain(via.next.iter())
                    .chain(via.check.iter())
                    .chain(via.item.iter())
                {
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
        let bulk = match owner {
            doc::constant_pool::PoolOwner::Function(function) if site.path.is_empty() => catalog
                .registered(&function)
                .and_then(|r| r.plan())
                .filter(|p| p.function == function)
                .and_then(|p| p.template.as_ref()?.heap.as_ref()?.copy),
            _ => None,
        };
        let mut source = Description {
            owner,
            site: site.clone(),
            body: Arc::clone(&source_body),
            params: params.into(),
            shared,
            persistent,
            cell_assignments: Ok(BTreeMap::new()),
            bulk,
            calls,
            room: SourceRoom {
                source: site,
                bulk: None,
                peak_extra: BTreeMap::new(),
                retained_delta: BTreeMap::new(),
                scratch: Box::new([]),
                exits: Box::new([]),
            },
        };
        source.cell_assignments = root_owners::cell_assignments(&source);
        source.room = derive::body(&source);
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
                for event in self.room.peak_extra.values() {
                    if event.point.phase == Phase::Test && matches!(&event.output, Some(Owner::Binding { declared, .. }) if *declared == child(&event.point.site, 1)) {
                        owners.insert(Owner::LoopCursor(event.point.site.clone()));
                        owners.insert(Owner::LoopHeader(event.point.site.clone()));
                    }
                    owners.extend(event.inputs.iter().cloned());
                    owners.extend(event.output.iter().cloned());
                    owners.extend(event.entities.iter().cloned());
                    for amount in [&event.peak, &event.delta] {
                        for measure in amount.terms.keys() {
                            match measure {
                                Measure::Local(owner) => {
                                    owners.insert(owner.clone());
                                }
                                Measure::Release(group)
                                | Measure::NativeRelease { owners: group, .. } => {
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
                let controls = owners.iter().filter(|owner| matches!(owner,
                    Owner::LoopCursor(_) | Owner::LoopHeader(_) | Owner::Departure(_)
                    | Owner::PendingDeparture(_) | Owner::ActiveDeparture(_))).cloned().collect();
                let slots = owners
                    .into_iter()
                    .enumerate()
                    .map(|(index, owner)| {
                        Ok((owner, u32::try_from(index).map_err(|_| Refused::Facts)?))
                    })
                    .collect::<Result<BTreeMap<_, _>, Refused>>()?;
                Ok(Arc::new(OwnerSlots {
                    owner: self.owner,
                    source: self.site.clone(),
                    slots,
                    controls,
                    scopes,
                    aliases,
                    binding_slots,
                }))
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
    pub(crate) fn room_if_derived(&self) -> Option<&SourceRoom> {
        self.description.get().map(|d| &d.room)
    }
    /// KIR formation does not consume memory authority in phase one. If a
    /// consumer has requested that authority, retain its independent check.
    pub(crate) fn verify_if_derived(&self) -> Result<(), Refused> {
        if self.description.get().is_some() {
            self.verify()
        } else {
            Ok(())
        }
    }
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

    /// The enclosing source consumer whose actual runtime journal must follow
    /// an expression published as an operand.
    pub(crate) fn transfer_target(&self, at: &Site) -> Option<TransferTarget> {
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return None;
        }
        let mut node = doc::Node::Block(&self.body);
        let mut site = self.site.clone();
        let mut parents = Vec::new();
        for index in &at.path[self.site.path.len()..] {
            parents.push((node, site.clone()));
            let child = node.children().get(*index as usize).copied()?;
            node = child;
            site = site.child(*index);
        }
        for (node, site) in parents.into_iter().rev() {
            match node {
                doc::Node::Expr(doc::Expr::List(_) | doc::Expr::Tuple(_)) => continue,
                doc::Node::Stmt(doc::Stmt::Let { .. }) => {
                    return Some(TransferTarget::Binding);
                }
                doc::Node::Stmt(doc::Stmt::Assign {
                    place: doc::Place::Variable(_),
                    ..
                }) => {
                    return Some(TransferTarget::Binding);
                }
                doc::Node::Stmt(doc::Stmt::Return { .. })
                    if site.path.len() == self.site.path.len() + 1 =>
                {
                    return Some(TransferTarget::RootReturn);
                }
                _ => return None,
            }
        }
        None
    }

    /// The first unpaid event after an expression's actual normal Commit.
    /// Source syntax and the canonical walker determine the successor. A
    /// required future branch/cursor decision declines instead of inventing it.
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
        if !matches!(node, doc::Node::Expr(_))
            || !self.room.peak_extra.contains_key(&point(at, Phase::Commit))
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
        if !next.passed || !self.room.peak_extra.contains_key(&successor) {
            return Err(Refused::Path);
        }
        cache.insert(at.clone(), successor.clone());
        Ok(successor)
    }

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
fn inputs(site: &Site, node: doc::Node<'_>) -> Box<[Owner]> {
    match node {
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

fn atoms(action: &doc::Action) -> &[doc::Atom] {
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

fn hook(target: &doc::FunctionTarget, calls: &BTreeMap<FunctionId, CallSource>) -> CallSource {
    match target {
        doc::FunctionTarget::Local(f) => calls[f],
        doc::FunctionTarget::Import(_) => CallSource::Dynamic,
    }
}
