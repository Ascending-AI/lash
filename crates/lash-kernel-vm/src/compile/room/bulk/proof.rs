//! Source value flow authenticates a finite collection result before storage.
//! Fact indices are source-local, independent of physical slots and family tags.
use super::*;
use crate::compile::{Entry as NativeEntry, LibId, LibRun};
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::kernel_internals::logical::{self, Slot};
use std::collections::BTreeSet;

#[path = "proof/evaluate.rs"]
mod evaluate;
pub(crate) use evaluate::PreparedStorage;

pub(super) enum ValueRule {
    Entry(Slot),
    OptionalEntry(Slot),
    Literal(Value),
    Copy(usize),
    EmptyTuple,
    Native {
        library: LibId,
        arguments: Box<[usize]>,
        after_member: bool,
    },
    Item {
        target: usize,
        index: usize,
    },
}
pub(super) struct EventRule {
    pub(super) at: Point,
    pub(super) measures: Box<[(Measure, Box<[usize]>)]>,
    pub(super) call: Option<(CallSource, usize)>,
    pub(super) condition: Option<usize>,
    pub(super) member: Option<[usize; 3]>,
}
pub(super) struct Compiled {
    pub(super) values: Box<[ValueRule]>,
    pub(super) events: Box<[EventRule]>,
    pub(super) entries: Box<[(Slot, usize)]>,
    pub(super) vacant: Box<[Slot]>,
    pub(super) survivors: Box<[(Slot, usize)]>,
    pub(super) written: Box<[Slot]>,
    pub(super) depth: u64,
}
struct Compiler<'a> {
    source: &'a Source,
    slots: Arc<OwnerSlots>,
    values: Vec<ValueRule>,
    live: BTreeMap<Slot, usize>,
    entries: BTreeMap<Slot, usize>,
    vacant: BTreeSet<Slot>,
    written: BTreeSet<Slot>,
    depth: u64,
    first: Point,
    member_seen: bool,
}
impl Compiler<'_> {
    fn slot(&self, owner: &Owner) -> Result<Slot, Refused> {
        match self.slots.root(owner, TaskId::MAIN, 0) {
            Some(logical::Owner::Value { slot, .. }) => Ok(slot),
            _ => Err(Refused::Source),
        }
    }
    fn value(&mut self, rule: ValueRule) -> usize {
        let index = self.values.len();
        self.values.push(rule);
        index
    }
    fn entry(&mut self, slot: Slot) -> Result<usize, Refused> {
        if let Some(&value) = self.live.get(&slot) {
            return Ok(value);
        }
        if !matches!(slot, Slot::Binding(_))
            || self.vacant.contains(&slot)
            || self.entries.contains_key(&slot)
        {
            return Err(Refused::Retained);
        }
        let value = self.value(ValueRule::Entry(slot));
        self.entries.insert(slot, value);
        self.live.insert(slot, value);
        Ok(value)
    }
    fn inputs(&mut self, event: &Event) -> Result<Vec<usize>, Refused> {
        event
            .inputs
            .iter()
            .map(|owner| {
                let slot = self.slot(owner)?;
                self.entry(slot)
            })
            .collect()
    }
    fn release(&mut self, owners: &[Owner]) -> Result<Vec<usize>, Refused> {
        let mut facts = BTreeMap::new();
        for owner in owners {
            if matches!(owner, Owner::Scope(_) | Owner::Operands(_)) {
                for root in self
                    .slots
                    .group(owner, TaskId::MAIN, 0)
                    .ok_or(Refused::Source)?
                    .iter()
                {
                    let logical::Owner::Value { slot, .. } = root else {
                        return Err(Refused::Source);
                    };
                    if let Some(&fact) = self.live.get(slot) {
                        facts.insert(*slot, fact);
                    } else if matches!(slot, Slot::Binding(_))
                        && !self.vacant.contains(slot)
                        && !self.entries.contains_key(slot)
                    {
                        // A containing scope includes declarations in arms
                        // that may never have been entered. Bind their actual
                        // initialized/vacant state, rather than invent a root.
                        let required = self.source.bindings.iter().find(|binding| matches!(self.source.binding_root(binding.index, TaskId::MAIN, 0), Some(logical::Owner::Value { slot: actual, .. }) if actual == *slot)).is_some_and(|binding| {
                            let declared = &binding.declared;
                            if declared == &self.source.site || self.first.site.path.starts_with(&declared.path) { return true; }
                            let Some((index, parent)) = declared.path.split_last() else { return true; };
                            self.first.site.path.starts_with(parent) && self.first.site.path.get(parent.len()).is_some_and(|first| index < first)
                        });
                        let fact = if required {
                            self.entry(*slot)?
                        } else {
                            let fact = self.value(ValueRule::OptionalEntry(*slot));
                            self.entries.insert(*slot, fact);
                            self.live.insert(*slot, fact);
                            fact
                        };
                        facts.insert(*slot, fact);
                    }
                }
            } else {
                let slot = self.slot(owner)?;
                let fact = self.entry(slot)?;
                facts.insert(slot, fact);
            }
        }
        Ok(facts.into_values().collect())
    }
    fn node<'a>(&'a self, at: &Point) -> Result<doc::Node<'a>, Refused> {
        let mut node = doc::Node::Block(&self.source.body);
        for index in &at.site.path[self.source.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        Ok(node)
    }
    fn binding(&self, at: &Site, name: &Name) -> Result<Slot, Refused> {
        let owner = root_owners::visible(&self.slots.scopes, at, name).ok_or(Refused::Source)?;
        self.slot(owner)
    }
    fn event(&mut self, exe: &Executable, event: &Event) -> Result<EventRule, Refused> {
        if !event.entities.is_empty() {
            return Err(Refused::Source);
        }
        let mut rule = EventRule {
            at: event.point.clone(),
            measures: Box::default(),
            call: None,
            condition: None,
            member: None,
        };
        // Measure release against the source state before this event. The
        // underlying entity set must survive the final boundary, checked below.
        let mut measures = Vec::new();
        for measure in event.peak.measures().chain(event.delta.measures()) {
            let owners = match measure {
                Measure::Release(owners) => Some(owners),
                _ => None,
            };
            if let Some(owners) = owners {
                let values = self.release(owners)?;
                if !measures.iter().any(|(m, _)| m == measure) {
                    measures.push((measure.clone(), values.into_boxed_slice()));
                }
            }
        }
        let inputs = self.inputs(event)?;
        let before = self.live.clone();
        let mut output = None;
        match event.point.phase {
            Phase::Enter => {
                if event.point.site == self.source.site {
                    return Err(Refused::Source);
                }
                // A loop cursor's binding already belongs to this scope at
                // Enter. Only source declarations/results establish vacancy.
            }
            Phase::Leave => {
                for owner in [
                    Owner::Scope(event.point.site.clone()),
                    Owner::Operands(event.point.site.clone()),
                ] {
                    for root in self
                        .slots
                        .group(&owner, TaskId::MAIN, 0)
                        .ok_or(Refused::Source)?
                        .iter()
                    {
                        let logical::Owner::Value { slot, .. } = root else {
                            return Err(Refused::Source);
                        };
                        self.live.remove(slot);
                    }
                }
            }
            Phase::Test => {
                let [condition] = inputs.as_slice() else {
                    return Err(Refused::Source);
                };
                rule.condition = Some(*condition);
            }
            Phase::Commit => {
                // Copy the small semantic selector before changing the fact map.
                match self.node(&event.point)? {
                    doc::Node::Expr(doc::Expr::Literal(_))
                    | doc::Node::Expr(doc::Expr::Variable(_)) => {
                        let input = self.source.scalar_input(&self.slots, &event.point)?;
                        output = Some(match input {
                            region::ScalarInput::Literal(value) => {
                                self.value(ValueRule::Literal(value))
                            }
                            region::ScalarInput::Binding(slot) => {
                                let input = self.entry(slot)?;
                                self.value(ValueRule::Copy(input))
                            }
                        });
                    }
                    doc::Node::Expr(doc::Expr::Tuple(items)) if items.is_empty() => {
                        output = Some(self.value(ValueRule::EmptyTuple));
                    }
                    doc::Node::Expr(doc::Expr::Call { function, args }) => {
                        let library = exe.library_lib(function).ok_or(Refused::Source)?;
                        let function = exe.lib(library);
                        if !matches!(function.run, LibRun::Native { .. })
                            || function.limit.is_some()
                            || function.shares_params
                            || args.len() != inputs.len()
                            || args.len() != function.arity
                            || !(function.specialization.native.immediate.is_some()
                                || function.length_read().is_some()
                                || function.item_read() == Some(doc::ValueKind::List))
                        {
                            return Err(Refused::Source);
                        }
                        if self.member_seen
                            && (function.length_read().is_some() || function.item_read().is_some())
                        {
                            return Err(Refused::Source);
                        }
                        self.depth =
                            self.depth.max(u64::from(function.operation())).max(
                                function.specialization.native.immediate.map_or(0, |entry| {
                                    u64::from(NativeEntry::Native(entry).depth())
                                }),
                            );
                        let value = self.value(ValueRule::Native {
                            library,
                            arguments: inputs.clone().into(),
                            after_member: self.member_seen,
                        });
                        rule.call = Some((function.specialization.resource, value));
                        output = Some(value);
                    }
                    doc::Node::Expr(doc::Expr::Member(member))
                        if matches!(member.as_ref(), doc::Member::Index { .. }) =>
                    {
                        if self.member_seen {
                            return Err(Refused::Source);
                        }
                        let [target, index] = inputs.as_slice() else {
                            return Err(Refused::Source);
                        };
                        output = Some(self.value(ValueRule::Item {
                            target: *target,
                            index: *index,
                        }));
                    }
                    doc::Node::Stmt(doc::Stmt::Let { name, .. }) => {
                        let name = name.clone();
                        if self.source.assigns_cell(&event.point.site) {
                            return Err(Refused::Source);
                        }
                        let [input] = inputs.as_slice() else {
                            return Err(Refused::Source);
                        };
                        let slot = self.slot(&Owner::Binding {
                            declared: event.point.site.clone(),
                            name,
                        })?;
                        if self.live.contains_key(&slot) {
                            return Err(Refused::Retained);
                        }
                        self.vacant.insert(slot);
                        self.live.insert(slot, *input);
                        self.written.insert(slot);
                    }
                    doc::Node::Stmt(doc::Stmt::Assign {
                        place: doc::Place::Variable(name),
                        ..
                    }) => {
                        let name = name.clone();
                        if self.source.assigns_cell(&event.point.site) {
                            return Err(Refused::Source);
                        }
                        let slot = self.binding(&event.point.site, &name)?;
                        self.entry(slot)?;
                        let [input] = inputs.as_slice() else {
                            return Err(Refused::Source);
                        };
                        self.live.insert(slot, *input);
                        self.written.insert(slot);
                    }
                    doc::Node::Stmt(doc::Stmt::Assign {
                        place: doc::Place::Member(doc::Member::Index { .. }),
                        ..
                    }) => {
                        self.member_seen = true;
                        rule.member =
                            Some(inputs.as_slice().try_into().map_err(|_| Refused::Source)?);
                    }
                    doc::Node::Stmt(doc::Stmt::If { .. } | doc::Stmt::Do { .. }) => {}
                    _ => return Err(Refused::Source),
                }
            }
            _ => return Err(Refused::Source),
        }
        for owner in &event.inputs {
            let slot = self.slot(owner)?;
            self.live.remove(&slot);
        }
        if let Some(output) = output {
            let slot = self.slot(event.output.as_ref().ok_or(Refused::Source)?)?;
            if self.live.insert(slot, output).is_some() {
                return Err(Refused::Retained);
            }
            if !self.entries.contains_key(&slot) {
                self.vacant.insert(slot);
            }
        }
        for measure in event.peak.measures().chain(event.delta.measures()) {
            match measure {
                Measure::Local(owner) => {
                    let slot = self.slot(owner)?;
                    let fact = self
                        .live
                        .get(&slot)
                        .or_else(|| before.get(&slot))
                        .copied()
                        .ok_or(Refused::Retained)?;
                    if !measures.iter().any(|(m, _)| m == measure) {
                        measures.push((measure.clone(), Box::new([fact])));
                    }
                }
                Measure::Members(_)
                    if output
                        .is_some_and(|at| matches!(self.values[at], ValueRule::EmptyTuple)) =>
                {
                    measures.push((measure.clone(), Box::default()));
                }
                Measure::Release(_)
                | Measure::CallPeak { .. }
                | Measure::CallDelta { .. }
                | Measure::NativeScratch { .. } => {}
                Measure::MemberDelta(_) | Measure::MemberGrowth(_) if rule.member.is_some() => {}
                _ => return Err(Refused::Facts),
            }
        }
        rule.measures = measures.into();
        Ok(rule)
    }
}
impl Compiled {
    pub(super) fn origin(&self, index: usize) -> Option<Slot> {
        match self.values[index] {
            ValueRule::Entry(slot) | ValueRule::OptionalEntry(slot) => Some(slot),
            ValueRule::Copy(input) => self.origin(input),
            _ => None,
        }
    }
    pub(super) fn compile(
        exe: &Executable,
        source: &Arc<Source>,
        events: &[Arc<CompiledInterval>],
    ) -> Result<Self, Refused> {
        let events = events.iter().flat_map(|interval| interval.events());
        let mut events = events.peekable();
        let mut cx = Compiler {
            source,
            slots: source.owner_slots()?,
            values: Vec::new(),
            live: BTreeMap::new(),
            entries: BTreeMap::new(),
            vacant: BTreeSet::new(),
            written: BTreeSet::new(),
            depth: 0,
            member_seen: false,
            first: events.peek().ok_or(Refused::Source)?.point.clone(),
        };
        let mut rules = Vec::new();
        for event in events {
            rules.push(cx.event(exe, event)?);
        }
        if rules.iter().filter(|rule| rule.member.is_some()).count() > 1 {
            return Err(Refused::Source);
        }
        Ok(Self {
            values: cx.values.into(),
            events: rules.into(),
            entries: cx.entries.into_iter().collect(),
            vacant: cx.vacant.into_iter().collect(),
            survivors: cx.live.into_iter().collect(),
            written: cx.written.into_iter().collect(),
            depth: cx.depth,
        })
    }
}

/// Dominance compares the complete union of ordered source facts. Signed
/// member/call deltas require equality; other measures are nonnegative.
pub(super) fn dominates(left: &Polynomial, right: &Polynomial) -> bool {
    use std::cmp::Ordering;
    if left.fixed < right.fixed {
        return false;
    }
    let mut left = left.terms.iter().peekable();
    let mut right = right.terms.iter().peekable();
    loop {
        let (key, a, b) = match (left.peek(), right.peek()) {
            (Some((a, _)), Some((b, _))) => match a.cmp(b) {
                Ordering::Less => {
                    let (key, value) = left.next().unwrap_or_else(|| unreachable!());
                    (key, *value, 0)
                }
                Ordering::Equal => {
                    let (key, a) = left.next().unwrap_or_else(|| unreachable!());
                    let (_, b) = right.next().unwrap_or_else(|| unreachable!());
                    (key, *a, *b)
                }
                Ordering::Greater => {
                    let (key, value) = right.next().unwrap_or_else(|| unreachable!());
                    (key, 0, *value)
                }
            },
            (Some(_), None) => {
                let (key, value) = left.next().unwrap_or_else(|| unreachable!());
                (key, *value, 0)
            }
            (None, Some(_)) => {
                let (key, value) = right.next().unwrap_or_else(|| unreachable!());
                (key, 0, *value)
            }
            (None, None) => return true,
        };
        if matches!(key.1, Measure::MemberDelta(_) | Measure::CallDelta { .. }) {
            if a != b {
                return false;
            }
        } else if a < b {
            return false;
        }
    }
}
