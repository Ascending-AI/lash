//! A selected source path compiled once into indexed logical algebra. Selection
//! validation walks source control only on a cache miss, never per execution.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Selection {
    Event { at: Point, exit: ExitKind },
    Branch { at: Site, taken: bool },
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key {
    event: bool,
    at: Site,
    bounds: Option<(Point, Point)>,
    selected: Box<[Selection]>,
}

struct Linear {
    fixed: i128,
    terms: Box<[(usize, i128)]>,
}
impl Linear {
    fn compile(amount: &Amount, variables: &BTreeMap<Measure, usize>) -> Self {
        Self {
            fixed: amount.fixed,
            terms: amount
                .terms
                .iter()
                .map(|(measure, coefficient)| (variables[measure], *coefficient))
                .collect(),
        }
    }
    fn evaluate(&self, facts: &[i128]) -> Result<i128, Refused> {
        self.terms
            .iter()
            .try_fold(self.fixed, |sum, (at, coefficient)| {
                sum.checked_add(facts[*at].checked_mul(*coefficient).ok_or(Refused::Facts)?)
                    .ok_or(Refused::Facts)
            })
    }
}
pub(super) struct PublicationSite {
    pub(super) assignment: Option<lash_kernel_value::Name>,
    pub(super) member: bool,
}
struct Step {
    publication: PublicationSite,
    event: Event,
    selected: ExitKind,
    normal: ExitKind,
    variables: Box<[(Measure, usize)]>,
    peak: Linear,
    delta: Linear,
}
impl Step {
    fn compile(
        source: &Source,
        event: Event,
        selected: ExitKind,
        variables: &mut usize,
    ) -> Result<Self, Refused> {
        let indices: BTreeMap<_, _> = event
            .peak
            .terms
            .keys()
            .chain(event.delta.terms.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .map(|measure| {
                let index = *variables;
                *variables += 1;
                (measure, index)
            })
            .collect();
        let mut node = doc::Node::Block(&source.body);
        for index in &event.point.site.path[source.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        let publication = PublicationSite {
            assignment: match node {
                doc::Node::Stmt(doc::Stmt::Assign {
                    place: doc::Place::Variable(name),
                    ..
                }) => Some(name.clone()),
                _ => None,
            },
            member: source.assigns_cell(&event.point.site)
                || matches!(
                    node,
                    doc::Node::Stmt(
                        doc::Stmt::Assign {
                            place: doc::Place::Member(_),
                            ..
                        } | doc::Stmt::Remove { .. }
                    )
                ),
        };
        Ok(Self {
            publication,
            peak: Linear::compile(&event.peak, &indices),
            delta: Linear::compile(&event.delta, &indices),
            normal: source
                .room
                .exits
                .iter()
                .find(|exit| exit.point == event.point)
                .ok_or(Refused::Exit)?
                .kind,
            variables: indices.into_iter().collect(),
            event,
            selected,
        })
    }
}
/// The fields and constructors are private. A schedule is not a source proof;
/// only the independent Source verifier and control walker form this program.
pub(crate) struct CompiledInterval {
    owner: doc::constant_pool::PoolOwner,
    source: Site,
    at: Site,
    route: Box<[Selection]>,
    steps: Box<[Step]>,
    variables: usize,
    exit: ExitKind,
    whole: bool,
}
impl Source {
    /// The exact runtime is at a defining block's direct Return. Its only
    /// successor is that block's Leave; earlier children have already completed.
    /// The canonical walker proves the suffix without replaying their history.
    pub(crate) fn compile_return_suffix(
        &self,
        at: &Point,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        if at.phase != Phase::Commit
            || at.site.unit != self.site.unit
            || at.site.path.len() != self.site.path.len() + 1
            || !at.site.path.starts_with(&self.site.path)
            || !matches!(
                self.body
                    .get(*at.site.path.last().ok_or(Refused::Path)? as usize),
                Some(doc::Stmt::Return { .. })
            )
        {
            return Err(Refused::Path);
        }
        let leave = Point {
            site: self.site.clone(),
            phase: Phase::Leave,
        };
        let route: Box<[_]> = [
            Selection::Event {
                at: at.clone(),
                exit: ExitKind::Return,
            },
            Selection::Event {
                at: leave.clone(),
                exit: ExitKind::Normal,
            },
        ]
        .into();
        let key = Key {
            event: true,
            at: at.site.clone(),
            bounds: Some((at.clone(), leave)),
            selected: route.clone(),
        };
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = cache.get(&key) {
            return Ok(Arc::clone(interval));
        }
        struct Suffix<'a> {
            source: &'a Site,
            returning: &'a Site,
            route: &'a [Selection],
            cursor: usize,
        }
        impl evaluate::Events for Suffix<'_> {
            fn completed(&mut self, site: &Site) -> Option<ExitKind> {
                (site != self.source && site != self.returning).then_some(ExitKind::Normal)
            }
            fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
                if site == self.source && phase == Phase::Enter {
                    return Ok(ExitKind::Normal);
                }
                let Some(Selection::Event { at, exit }) = self.route.get(self.cursor) else {
                    return Err(Refused::Path);
                };
                if &at.site != site || at.phase != phase {
                    return Err(Refused::Path);
                }
                self.cursor += 1;
                Ok(*exit)
            }
            fn choose(&mut self, _: &Site) -> Result<bool, Refused> {
                Err(Refused::Facts)
            }
            fn selected_end(&self) -> bool {
                self.cursor == self.route.len()
            }
        }
        let mut suffix = Suffix {
            source: &self.site,
            returning: &at.site,
            route: &route,
            cursor: 0,
        };
        let exit = evaluate::walk_to(self, &self.site, &mut suffix, 3)?;
        if exit != ExitKind::Return || suffix.cursor != route.len() {
            return Err(Refused::Path);
        }
        let mut variables = 0;
        let steps = route
            .iter()
            .map(|selection| {
                let Selection::Event { at, exit } = selection else {
                    return Err(Refused::Path);
                };
                Step::compile(
                    self,
                    self.room.peak_extra.get(at).ok_or(Refused::Path)?.clone(),
                    *exit,
                    &mut variables,
                )
            })
            .collect::<Result<Box<[_]>, Refused>>()?;
        let interval = Arc::new(CompiledInterval {
            owner: self.owner,
            source: self.site.clone(),
            at: at.site.clone(),
            route,
            steps,
            variables,
            exit,
            whole: false,
        });
        cache.insert(key, Arc::clone(&interval));
        Ok(interval)
    }

    /// Compile the first unpaid event `from` through the included `through`
    /// exit. `selected` starts at the endpoint sites' lowest common ancestor
    /// and ends at `through`. Its evaluated prefix authenticates control only
    /// and is never replayed. The first occurrence of `from` is the entry;
    /// `through` is the last selected event.
    pub(crate) fn compile_between(
        &self,
        from: &Point,
        through: &Point,
        selected: &[Selection],
        budget: usize,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        if selected.len() > budget.min(4096)
            || !self.room.peak_extra.contains_key(from)
            || !self.room.peak_extra.contains_key(through)
            || !matches!(selected.last(), Some(Selection::Event { at, .. }) if at == through)
        {
            return Err(Refused::Path);
        }
        let key = Key {
            event: false,
            at: from.site.clone(),
            bounds: Some((from.clone(), through.clone())),
            selected: selected.into(),
        };
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = cache.get(&key) {
            return Ok(Arc::clone(interval));
        }
        if cache.keys().filter(|key| !key.event).count() >= 128 {
            return Err(Refused::Path);
        }
        let mut context = from.site.clone();
        let common = from
            .site
            .path
            .iter()
            .zip(&through.site.path)
            .take_while(|(a, b)| a == b)
            .count();
        context.path.truncate(common);
        let mut trace = Trace {
            source: self,
            selected,
            cursor: 0,
            events: Vec::new(),
            stop: true,
        };
        let exit = evaluate::walk_to(self, &context, &mut trace, budget)?;
        if trace.cursor != selected.len() {
            return Err(Refused::Path);
        }
        let first = trace
            .events
            .iter()
            .position(|(event, _)| event.point == *from)
            .ok_or(Refused::Path)?;
        let route = selected
            .iter()
            .position(|selection| matches!(selection, Selection::Event { at, .. } if at == from))
            .ok_or(Refused::Path)?;
        let mut variables = 0;
        let mut steps = Vec::with_capacity(trace.events.len() - first);
        for (event, selected) in trace.events.into_iter().skip(first) {
            steps.push(Step::compile(self, event, selected, &mut variables)?);
        }
        let interval = Arc::new(CompiledInterval {
            owner: self.owner,
            source: self.site.clone(),
            at: from.site.clone(),
            route: selected[route..].into(),
            steps: steps.into(),
            variables,
            exit,
            whole: false,
        });
        cache.insert(key, Arc::clone(&interval));
        Ok(interval)
    }

    /// Exact execution is already at this authenticated source point. Compile
    /// its one event, never a helper body or a numeric replacement for a path.
    /// The finite source event set is cached separately from selected routes.
    pub(crate) fn compile_event(
        &self,
        at: &Point,
        selected: ExitKind,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        if !self
            .room
            .exits
            .iter()
            .any(|exit| exit.point == *at && exit.kind == selected)
        {
            return Err(Refused::Exit);
        }
        let key = Key {
            event: true,
            at: at.site.clone(),
            bounds: None,
            selected: vec![Selection::Event {
                at: at.clone(),
                exit: selected,
            }]
            .into(),
        };
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = cache.get(&key) {
            return Ok(Arc::clone(interval));
        }
        let event = self.room.peak_extra.get(at).ok_or(Refused::Path)?.clone();
        let mut variables = 0;
        let step = Step::compile(self, event, selected, &mut variables)?;
        let interval = Arc::new(CompiledInterval {
            owner: self.owner,
            source: self.site.clone(),
            at: at.site.clone(),
            route: key.selected.clone(),
            steps: vec![step].into(),
            variables,
            exit: selected,
            whole: false,
        });
        cache.insert(key, Arc::clone(&interval));
        Ok(interval)
    }
    pub(crate) fn compile_interval(
        &self,
        at: &Site,
        selected: &[Selection],
        budget: usize,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        if selected.len() > budget.min(4096) {
            return Err(Refused::Path);
        }
        let key = Key {
            event: false,
            at: at.clone(),
            bounds: None,
            selected: selected.into(),
        };
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = cache.get(&key) {
            return Ok(Arc::clone(interval));
        }
        if cache.keys().filter(|key| !key.event).count() >= 128 {
            return Err(Refused::Path);
        }
        let mut trace = Trace {
            source: self,
            selected,
            cursor: 0,
            events: Vec::new(),
            stop: false,
        };
        let exit = evaluate::walk(self, at, &mut trace, budget)?;
        if trace.cursor != selected.len() {
            return Err(Refused::Path);
        }
        let mut variables = 0;
        let mut steps = Vec::with_capacity(trace.events.len());
        for (event, selected) in trace.events {
            steps.push(Step::compile(self, event, selected, &mut variables)?);
        }
        let interval = Arc::new(CompiledInterval {
            owner: self.owner,
            source: self.site.clone(),
            at: at.clone(),
            route: selected.into(),
            steps: steps.into(),
            variables,
            exit,
            whole: at == &self.site,
        });
        cache.insert(key, Arc::clone(&interval));
        Ok(interval)
    }
}
struct Trace<'a> {
    source: &'a Source,
    selected: &'a [Selection],
    cursor: usize,
    events: Vec<(Event, ExitKind)>,
    stop: bool,
}
impl evaluate::Events for Trace<'_> {
    fn selected_end(&self) -> bool {
        self.stop && self.cursor == self.selected.len()
    }
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let expected = point(site, phase);
        let Some(Selection::Event { at, exit }) = self.selected.get(self.cursor) else {
            return Err(Refused::Path);
        };
        if *at != expected
            || !self
                .source
                .room
                .exits
                .iter()
                .any(|allowed| allowed.point == *at && allowed.kind == *exit)
        {
            return Err(Refused::Exit);
        }
        let event = self.source.room.peak_extra.get(at).ok_or(Refused::Path)?;
        self.events.push((event.clone(), *exit));
        self.cursor += 1;
        Ok(*exit)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        let Some(Selection::Branch { at, taken }) = self.selected.get(self.cursor) else {
            return Err(Refused::Path);
        };
        if at != site {
            return Err(Refused::Path);
        }
        self.cursor += 1;
        Ok(*taken)
    }
}
impl CompiledInterval {
    pub(crate) fn owner(&self) -> doc::constant_pool::PoolOwner {
        self.owner
    }

    pub(crate) fn exit(&self) -> ExitKind {
        self.exit
    }
    pub(crate) fn defining_source(&self) -> &Site {
        &self.source
    }
    pub(crate) fn start(&self) -> &Site {
        &self.at
    }
    pub(crate) fn events(&self) -> impl ExactSizeIterator<Item = &Event> {
        self.steps.iter().map(|step| &step.event)
    }
    /// Bind canonical event facts once, then evaluate indexed polynomials.
    /// No source node, child vector, or AST traversal is used here.
    pub(crate) fn evaluate_interval(
        &self,
        execution: &mut impl Execution,
    ) -> Result<CheckedRoom, Refused> {
        self.evaluate_checked(execution, |_, _, _| Ok(()))
    }
    pub(super) fn evaluate_checked(
        &self,
        execution: &mut impl Execution,
        mut check: impl FnMut(&Event, &PublicationSite, &dyn Facts) -> Result<(), Refused>,
    ) -> Result<CheckedRoom, Refused> {
        let mut values = vec![0; self.variables];
        let mut balance = 0_i128;
        let mut peak = 0_i128;
        let mut steps = self.steps.iter();
        for selection in &self.route {
            match selection {
                Selection::Branch { at, taken } => {
                    if execution.choose(at) != Some(*taken) {
                        return Err(Refused::Path);
                    }
                }
                Selection::Event { at, .. } => {
                    let step = steps.next().ok_or(Refused::Path)?;
                    let facts = execution.event(at).ok_or(Refused::Facts)?;
                    check(&step.event, &step.publication, facts.as_ref())?;
                    for (measure, index) in &step.variables {
                        values[*index] =
                            Amount::measure(measure, facts.as_ref()).ok_or(Refused::Facts)?;
                    }
                    let actual = event_exit(&step.event, facts.as_ref(), step.normal)?;
                    if actual != step.selected {
                        return Err(Refused::Exit);
                    }
                    let extra = step.peak.evaluate(&values)?;
                    if extra < 0 {
                        return Err(Refused::Peak);
                    }
                    peak = peak.max(balance.checked_add(extra).ok_or(Refused::Facts)?);
                    balance = balance
                        .checked_add(step.delta.evaluate(&values)?)
                        .ok_or(Refused::Facts)?;
                    peak = peak.max(balance);
                }
            }
        }
        Ok(CheckedRoom {
            owner: self.owner,
            native: false,
            whole: self.whole,
            native_contract: None,
            balance: Balance {
                peak_extra: u64::try_from(peak.max(0)).map_err(|_| Refused::Facts)?,
                retained_delta: balance,
            },
            exit: self.exit,
        })
    }
}
fn event_exit(event: &Event, facts: &dyn Facts, normal: ExitKind) -> Result<ExitKind, Refused> {
    if facts.raised() {
        if event.output.is_none() {
            return Err(Refused::Facts);
        }
        return Ok(ExitKind::Raise);
    }
    for measure in event.peak.terms.keys() {
        if let Measure::CallPeak { at, callee } = measure {
            return Ok(
                if facts.call(at, *callee).ok_or(Refused::Facts)?.exit == ExitKind::Raise {
                    ExitKind::Raise
                } else {
                    ExitKind::Normal
                },
            );
        }
    }
    Ok(normal)
}
