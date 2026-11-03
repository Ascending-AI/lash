//! A selected source path compiled once into indexed logical algebra. Selection
//! validation walks source control only on a cache miss, never per execution.
use super::*;
#[path = "publication/exact/construction.rs"]
pub(in crate::compile::room) mod construction;
mod continuation;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Selection {
    Event { at: Point, exit: ExitKind },
    Branch { at: Site, taken: bool },
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key {
    event: bool,
    call: bool,
    join: Option<Site>,
    at: Site,
    bounds: Option<(Point, Point)>,
    selected: Box<[Selection]>,
}

/// Single source events by their point: each point has at most a few
/// selected exits. A hit was validated when it was compiled.
pub(super) type Events = std::collections::HashMap<Point, Vec<(ExitKind, Arc<CompiledInterval>)>>;
/// Authenticated leaf reads by their site: the read event and its next
/// unpaid point.
pub(super) type Leaves = std::collections::HashMap<Site, (Point, Point, Arc<CompiledInterval>)>;

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
    pub(super) construction: Option<construction::ConstructionEvent>,
    pub(super) borrowed_inputs: bool,
    pub(super) restored_roles: Option<Box<[Site]>>,
    pub(super) uncounted_resumption: bool,
    pub(super) assignment: Option<lash_kernel_value::Name>,
    pub(super) member: bool,
    pub(super) field: Option<Arc<str>>,
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
            .measures()
            .chain(event.delta.measures())
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
        let restores_call = match node {
            doc::Node::Expr(doc::Expr::Act(act)) => matches!(act.kind, doc::ActKind::Call(_)),
            doc::Node::Expr(doc::Expr::Call { function, .. }) => {
                matches!(source.calls.get(function), Some(CallSource::Body(_)))
            }
            _ => false,
        };
        let restored_roles = if event.point.phase == Phase::Restore && restores_call {
            let relative = Site::new(
                event.point.site.unit.clone(),
                event.point.site.path[source.site.path.len()..].to_vec(),
            );
            Some(
                doc::live_roles_in(&source.body, &relative)
                    .map_err(|_| Refused::Source)?
                    .into_iter()
                    .map(|role| {
                        let mut path = source.site.path.clone();
                        path.extend(role.path);
                        Site::new(role.unit, path)
                    })
                    .collect(),
            )
        } else {
            None
        };
        let publication = PublicationSite {
            construction: construction::ConstructionEvent::compile(source, &event.point)?,
            uncounted_resumption: event.point.phase == Phase::Restore
                && matches!(node, doc::Node::Expr(doc::Expr::Call { .. })),
            restored_roles,
            borrowed_inputs: event.point.phase == Phase::Enter
                && matches!(node, doc::Node::Expr(doc::Expr::Act(_))),
            assignment: match node {
                doc::Node::Stmt(doc::Stmt::Assign {
                    place: doc::Place::Variable(name),
                    ..
                }) => Some(name.clone()),
                _ => None,
            },
            field: match node {
                doc::Node::Stmt(
                    doc::Stmt::Assign {
                        place: doc::Place::Member(doc::Member::Field { field, .. }),
                        ..
                    }
                    | doc::Stmt::Remove {
                        member: doc::Member::Field { field, .. },
                    },
                ) => Some(Arc::from(field.as_str())),
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
    return_loops: Option<Box<[Site]>>,
    resume: Option<Point>,
    /// Whether this interval is a proven acquisition: decided once, since
    /// an interval belongs to the one Source that compiled it.
    acquires: std::sync::OnceLock<bool>,
    /// Whether this interval is a proven native commit, decided once.
    commits: std::sync::OnceLock<bool>,
}
impl Source {
    /// An issued Return leaves its entered ordinary blocks, then its Source.
    /// Its authored path proves enclosing if choices; earlier children have
    /// completed. Entered loop bodies carry their actual occurrence; their
    /// cursor/header prefix has completed. A try is left by its entered body
    /// or catch block; an entered catch follows a raised body. The innermost
    /// cleanup ends the suffix at its Suspend, which holds the departure.
    /// A Break or Continue departs the same way toward its innermost loop;
    /// a cleanup inside that loop suspends it, otherwise the loop's Catch
    /// takes it. A Throw departs toward the innermost try it reaches, whose
    /// catch takes it or whose cleanup suspends it.
    pub(crate) fn compile_departure_suffix(
        &self,
        at: &Point,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        if at.phase != Phase::Commit
            || at.site.unit != self.site.unit
            || !at.site.path.starts_with(&self.site.path)
            || at.site.path.len() <= self.site.path.len()
        {
            return Err(Refused::Path);
        }
        let mut node = doc::Node::Block(&self.body);
        let mut path = self.site.clone();
        let mut blocks = Vec::new();
        let mut choices = BTreeMap::new();
        let mut return_loops = Vec::new();
        let mut for_sites = std::collections::BTreeSet::new();
        let mut raised = std::collections::BTreeSet::new();
        let mut suspend: Option<(Site, Site)> = None;
        let mut target: Option<Site> = None;
        let mut target_while = false;
        // The innermost try a raise reaches takes it: true by its catch.
        let mut handler: Option<(Site, bool)> = None;
        for index in &at.site.path[self.site.path.len()..] {
            match node {
                doc::Node::Block(_) => blocks.push(path.clone()),
                doc::Node::Stmt(doc::Stmt::If { .. }) if matches!(*index, 1 | 2) => {
                    choices.insert(path.clone(), *index == 1);
                }
                doc::Node::Stmt(doc::Stmt::While { .. }) if *index == 1 => {
                    choices.insert(path.clone(), true);
                    return_loops.push(path.clone());
                    target = Some(path.clone());
                    target_while = true;
                }
                doc::Node::Stmt(doc::Stmt::For { .. }) if *index == 1 => {
                    choices.insert(path.clone(), true);
                    return_loops.push(path.clone());
                    for_sites.insert(path.clone());
                    target = Some(path.clone());
                    target_while = false;
                }
                doc::Node::Stmt(doc::Stmt::Try(t))
                    if *index == 0 || (t.catch.is_some() && *index == 1) =>
                {
                    if *index == 1 {
                        raised.insert(path.clone());
                    }
                    if *index == 0 && t.catch.is_some() {
                        handler = Some((path.clone(), true));
                    } else if t.finally.is_some() {
                        handler = Some((path.clone(), false));
                    }
                    if t.finally.is_some() {
                        let cleanup = path.child(if t.catch.is_some() { 2 } else { 1 });
                        suspend = Some((path.clone(), cleanup));
                    }
                }
                // A return inside cleanup supersedes its pending departure.
                _ => return Err(Refused::Path),
            }
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
            path = path.child(*index);
        }
        let departing = match node {
            doc::Node::Stmt(doc::Stmt::Return { .. }) => ExitKind::Return,
            doc::Node::Stmt(doc::Stmt::Break) => ExitKind::Break,
            doc::Node::Stmt(doc::Stmt::Continue) => ExitKind::Continue,
            doc::Node::Stmt(doc::Stmt::Throw { .. }) => ExitKind::Raise,
            _ => return Err(Refused::Path),
        };
        // A raise publishes into the innermost try it reaches: its catch
        // takes it, or its cleanup suspends it. The caller takes the others.
        let mut caught = None;
        if departing == ExitKind::Raise {
            match (&handler, &suspend) {
                (Some((try_site, true)), _) => {
                    suspend = None;
                    caught = Some(try_site.clone());
                }
                (Some((try_site, false)), Some((cleanup_try, _))) if try_site == cleanup_try => {}
                _ => return Err(Refused::Path),
            }
        }
        // A break or continue whose target loop holds no cleanup inside it
        // ends at that loop's Catch, which takes the value-less record.
        if matches!(departing, ExitKind::Break | ExitKind::Continue) {
            let target = target.ok_or(Refused::Path)?;
            if suspend
                .as_ref()
                .is_none_or(|(try_site, _)| try_site.path.len() < target.path.len())
            {
                suspend = None;
                caught = Some(target);
            }
        }
        let resume = match (&suspend, &caught) {
            (Some((_, cleanup)), _) => Some(self.first_event(cleanup)?),
            (None, Some(try_site)) if departing == ExitKind::Raise => {
                Some(self.first_event(&child(try_site, 1))?)
            }
            (None, Some(target)) if departing == ExitKind::Break => Some(
                self.room
                    .events
                    .get(&point(target, Phase::Commit))
                    .ok_or(Refused::Path)?
                    .point
                    .clone(),
            ),
            (None, Some(target)) if target_while => Some(self.first_event(&child(target, 0))?),
            // A for's next head depends on its actual cursor.
            (None, Some(_)) => None,
            (None, None) => None,
        };
        let suspend = suspend.map(|(try_site, _)| try_site);
        let (end, bound) = match (&suspend, &caught) {
            (Some(try_site), _) => (
                Point {
                    site: try_site.clone(),
                    phase: Phase::Suspend,
                },
                Some(try_site),
            ),
            (None, Some(target)) => (
                Point {
                    site: target.clone(),
                    phase: Phase::Catch,
                },
                Some(target),
            ),
            (None, None) => (
                Point {
                    site: self.site.clone(),
                    phase: Phase::Leave,
                },
                None,
            ),
        };
        let route: Box<[_]> = std::iter::once(Selection::Event {
            at: at.clone(),
            exit: departing,
        })
        .chain(
            blocks
                .iter()
                .rev()
                .take_while(|site| bound.is_none_or(|bound| site.path.len() > bound.path.len()))
                .map(|site| Selection::Event {
                    at: Point {
                        site: site.clone(),
                        phase: Phase::Leave,
                    },
                    exit: ExitKind::Normal,
                }),
        )
        .chain(bound.is_some().then(|| Selection::Event {
            at: end.clone(),
            exit: ExitKind::Normal,
        }))
        .collect();
        let key = Key {
            event: true,
            call: false,
            join: None,
            at: at.site.clone(),
            bounds: Some((at.clone(), end)),
            selected: route.clone(),
        };
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = cache.get(&key) {
            return Ok(Arc::clone(interval));
        }
        struct Suffix<'a> {
            returning: &'a Site,
            blocks: &'a [Site],
            choices: &'a BTreeMap<Site, bool>,
            for_sites: &'a std::collections::BTreeSet<Site>,
            raised: &'a std::collections::BTreeSet<Site>,
            route: &'a [Selection],
            cursor: usize,
        }
        impl evaluate::Events for Suffix<'_> {
            fn completed(&mut self, site: &Site) -> Option<ExitKind> {
                if self.returning.path.starts_with(&site.path) {
                    return None;
                }
                // The body of an entered catch has already raised.
                Some(match site.path.split_last() {
                    Some((0, parent))
                        if self.raised.iter().any(|try_site| {
                            try_site.unit == site.unit && try_site.path == parent
                        }) =>
                    {
                        ExitKind::Raise
                    }
                    _ => ExitKind::Normal,
                })
            }
            fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
                if self.cursor == 0
                    && ((phase == Phase::Enter && self.blocks.contains(site))
                        || (phase == Phase::Test && self.choices.contains_key(site))
                        || (phase == Phase::Catch && self.raised.contains(site))
                        || (self.for_sites.contains(site)
                            && matches!(
                                phase,
                                Phase::Open | Phase::Next | Phase::Check | Phase::Item
                            )))
                {
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
            fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
                self.choices.get(site).copied().ok_or(Refused::Facts)
            }
            fn selected_end(&self) -> bool {
                self.cursor == self.route.len()
            }
        }
        let mut suffix = Suffix {
            returning: &at.site,
            blocks: &blocks,
            choices: &choices,
            for_sites: &for_sites,
            raised: &raised,
            route: &route,
            cursor: 0,
        };
        let exit = evaluate::walk_to(
            self,
            &self.site,
            &mut suffix,
            route.len() + blocks.len() + choices.len() + 4 * for_sites.len() + raised.len(),
        )?;
        if exit != departing || suffix.cursor != route.len() {
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
                    self.room.events.get(at).ok_or(Refused::Path)?.clone(),
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
            return_loops: Some(return_loops.into()),
            resume,
            acquires: std::sync::OnceLock::new(),
            commits: std::sync::OnceLock::new(),
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
            || !self.room.events.contains_key(from)
            || !self.room.events.contains_key(through)
            || !matches!(selected.last(), Some(Selection::Event { at, .. }) if at == through)
        {
            return Err(Refused::Path);
        }
        let key = Key {
            event: false,
            call: false,
            join: None,
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
            return_loops: None,
            resume: None,
            acquires: std::sync::OnceLock::new(),
            commits: std::sync::OnceLock::new(),
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
        self.compile_event_input(at, selected, None)
    }

    /// The source tree authenticates both the read event and its next unpaid
    /// point. A value callee is held before arguments, at Open, rather than at
    /// the action's later result Commit.
    pub(crate) fn compile_leaf(
        &self,
        at: &Site,
    ) -> Result<(Point, Point, Arc<CompiledInterval>), Refused> {
        self.verify()?;
        if let Some(leaf) = self.leaves.lock().map_err(|_| Refused::Source)?.get(at) {
            return Ok(leaf.clone());
        }
        if at.unit != self.site.unit || !at.path.starts_with(&self.site.path) {
            return Err(Refused::Path);
        }
        let mut node = doc::Node::Block(&self.body);
        for index in &at.path[self.site.path.len()..] {
            node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
        }
        let (event, unpaid) = match node {
            doc::Node::Expr(doc::Expr::Literal(_) | doc::Expr::Variable(_)) => {
                (point(at, Phase::Commit), self.next_after_commit(at)?)
            }
            doc::Node::Expr(doc::Expr::Act(act))
                if matches!(
                    act.kind,
                    doc::ActKind::Call(doc::Callee::Value(_))
                        | doc::ActKind::Spawn(doc::Callee::Value(_))
                ) =>
            {
                let unpaid = if act.args.is_empty() {
                    point(at, Phase::Enter)
                } else {
                    self.first_event(&child(at, 0))?
                };
                (point(at, Phase::Open), unpaid)
            }
            _ => return Err(Refused::Path),
        };
        let interval = self.compile_event(&event, ExitKind::Normal)?;
        let leaf = (event, unpaid, interval);
        self.leaves
            .lock()
            .map_err(|_| Refused::Source)?
            .insert(at.clone(), leaf.clone());
        Ok(leaf)
    }

    /// Exact execution supplies the selected child's authenticated operand.
    /// This is the source expression's Commit, with its unchanged algebra.
    pub(crate) fn compile_join(
        &self,
        from: &Site,
        to: &Site,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.compile_event_input(&point(to, Phase::Commit), ExitKind::Normal, Some(from))
    }

    fn compile_event_input(
        &self,
        at: &Point,
        selected: ExitKind,
        from: Option<&Site>,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        if from.is_none()
            && let Some(compiled) = self.events.lock().map_err(|_| Refused::Source)?.get(at)
            && let Some((_, interval)) = compiled.iter().find(|(exit, _)| *exit == selected)
        {
            return Ok(Arc::clone(interval));
        }
        let route: Box<[Selection]> = vec![Selection::Event {
            at: at.clone(),
            exit: selected,
        }]
        .into();
        let key = from.map(|from| Key {
            event: true,
            call: false,
            join: Some(from.clone()),
            at: at.site.clone(),
            bounds: None,
            selected: route.clone(),
        });
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = key.as_ref().and_then(|key| cache.get(key)) {
            return Ok(Arc::clone(interval));
        }
        if let Some(from) = from {
            if at.phase != Phase::Commit
                || at.site.unit != self.site.unit
                || !at.site.path.starts_with(&self.site.path)
            {
                return Err(Refused::Path);
            }
            let mut node = doc::Node::Block(&self.body);
            for index in &at.site.path[self.site.path.len()..] {
                node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
            }
            match node {
                doc::Node::Expr(doc::Expr::If { .. })
                    if *from == child(&at.site, 1) || *from == child(&at.site, 2) => {}
                doc::Node::Expr(doc::Expr::Let { .. }) if *from == child(&at.site, 1) => {}
                _ => return Err(Refused::Path),
            }
        }
        if !self
            .room
            .exits
            .iter()
            .any(|exit| exit.point == *at && exit.kind == selected)
        {
            return Err(Refused::Exit);
        }
        let mut event = self.room.events.get(at).ok_or(Refused::Path)?.clone();
        if let Some(from) = from {
            event.inputs = vec![operand(from)].into();
        }
        let mut variables = 0;
        let step = Step::compile(self, event, selected, &mut variables)?;
        let interval = Arc::new(CompiledInterval {
            owner: self.owner,
            source: self.site.clone(),
            at: at.site.clone(),
            route,
            steps: vec![step].into(),
            variables,
            exit: selected,
            whole: false,
            return_loops: None,
            resume: None,
            acquires: std::sync::OnceLock::new(),
            commits: std::sync::OnceLock::new(),
        });
        match key {
            Some(key) => {
                cache.insert(key, Arc::clone(&interval));
            }
            None => {
                drop(cache);
                self.events
                    .lock()
                    .map_err(|_| Refused::Source)?
                    .entry(at.clone())
                    .or_default()
                    .push((selected, Arc::clone(&interval)));
            }
        }
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
            call: false,
            join: None,
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
            return_loops: None,
            resume: None,
            acquires: std::sync::OnceLock::new(),
            commits: std::sync::OnceLock::new(),
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
        let event = self.source.room.events.get(at).ok_or(Refused::Path)?;
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
    pub(super) fn selections(&self) -> &[Selection] {
        &self.route
    }
    pub(crate) fn return_loops(&self) -> Option<&[Site]> {
        self.return_loops.as_deref()
    }
    /// A suspended departure resumes at its cleanup block's first event; a
    /// caught raise at its catch block, a caught break at its loop's Commit,
    /// a caught while continue at its condition. A caught for continue resumes where its cursor goes.
    pub(crate) fn resume(&self) -> Option<&Point> {
        self.resume.as_ref()
    }

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
    pub(crate) fn restored_roles(&self) -> Option<&[Site]> {
        let [step] = self.steps.as_ref() else {
            return None;
        };
        step.publication.restored_roles.as_deref()
    }
    pub(crate) fn events(&self) -> impl ExactSizeIterator<Item = &Event> {
        self.steps.iter().map(|step| &step.event)
    }
    /// A proven pure acquisition: one normal event that only acquires its own
    /// operand owner, with peak and retained delta exactly that owner's local
    /// bytes, no inputs, releases, entities, members or scratch. For such an
    /// interval the checked journal is fully determined by the value's holding,
    /// so its shared admission is the memory envelope and charge window alone.
    pub(crate) fn acquisition(&self, source: &Source) -> Option<&Owner> {
        let owner = self.steps.first()?.event.output.as_ref()?;
        self.acquires
            .get_or_init(|| self.acquires_in(source))
            .then_some(owner)
    }
    fn acquires_in(&self, source: &Source) -> bool {
        let ([Selection::Event { .. }], [step]) = (self.route.as_ref(), self.steps.as_ref()) else {
            return false;
        };
        let event = &step.event;
        let publication = &step.publication;
        let Some(owner) = event.output.as_ref() else {
            return false;
        };
        let local = |amount: &Amount| {
            amount.fixed == 0
                && matches!(amount.terms.as_slice(), [(Measure::Local(at), 1)] if at == owner)
        };
        (matches!(owner, Owner::Operand(site) if *site == event.point.site)
            && event.point.site != source.site
            && step.selected == ExitKind::Normal
            && step.normal == ExitKind::Normal
            && event.inputs.is_empty()
            && event.entities.is_empty()
            && local(&event.peak)
            && local(&event.delta)
            && !publication.borrowed_inputs
            && publication.restored_roles.is_none()
            && publication.assignment.is_none()
            && !publication.member
            && publication.field.is_none()
            && !source
                .room
                .scratch
                .iter()
                .any(|lease| lease.at == event.point))
    }
    /// A proven native commit: one normal Commit event whose output is its
    /// own operand owner, whose peak is exactly the native call's peak (or,
    /// for an operator act, its answer's holding) and whose retained delta is
    /// that measure's delta less its consumed operands,
    /// with no entity, member, field, cell or restored role. Its scratch
    /// lease is the call's runtime scratch, which the caller requires to be
    /// zero. For such an interval with a scalar answer the checked journal releases
    /// the operands and holds the answer, so its shared admission is the
    /// call's envelope, charge window and depth alone.
    pub(crate) fn native_commit(&self, source: &Source) -> bool {
        *self.commits.get_or_init(|| self.commits_natively(source))
    }
    fn commits_natively(&self, source: &Source) -> bool {
        let ([Selection::Event { .. }], [step]) = (self.route.as_ref(), self.steps.as_ref()) else {
            return false;
        };
        let event = &step.event;
        let publication = &step.publication;
        let site = &event.point.site;
        // The callee's own peak, or the answer's holding for an operator
        // act; the retained delta is that same measure less the consumed
        // operands.
        let produced = |measure: &Measure| match measure {
            Measure::CallPeak {
                at,
                callee: CallSource::Native(_),
            } => at == site,
            Measure::Local(Owner::Operand(at)) => at == site,
            _ => false,
        };
        let retained = |measure: &Measure| match measure {
            Measure::CallDelta {
                at,
                callee: CallSource::Native(_),
            } => at == site,
            Measure::Local(Owner::Operand(at)) => at == site,
            _ => false,
        };
        let consumed = |measure: &Measure| matches!(measure, Measure::Release(owners) if **owners == *event.inputs);
        let peak = matches!(event.peak.terms.as_slice(), [(measure, 1)] if produced(measure));
        let delta = match event.delta.terms.as_slice() {
            [(measure, 1)] => retained(measure) && event.inputs.is_empty(),
            [(first, 1), (second, -1)] => retained(first) && consumed(second),
            [(first, -1), (second, 1)] => consumed(first) && retained(second),
            _ => false,
        };
        event.point.phase == Phase::Commit
            && event.point.site != source.site
            && step.selected == ExitKind::Normal
            && step.normal == ExitKind::Normal
            && matches!(&event.output, Some(Owner::Operand(at)) if at == site)
            && event.entities.is_empty()
            && event.peak.fixed == 0
            && event.delta.fixed == 0
            && peak
            && delta
            && !publication.borrowed_inputs
            && publication.restored_roles.is_none()
            && publication.assignment.is_none()
            && !publication.member
            && publication.field.is_none()
    }
    /// A nested scope's Leave is a proven release when its one event only
    /// releases that scope's own owner groups: no output, entity, input,
    /// peak, scratch lease or publication role, and both exits normal.
    pub(crate) fn scope_release(&self, source: &Source, closing: &Site) -> bool {
        let ([Selection::Event { .. }], [step]) = (self.route.as_ref(), self.steps.as_ref()) else {
            return false;
        };
        let event = &step.event;
        let publication = &step.publication;
        let release = Measure::Release(
            vec![
                Owner::Scope(closing.clone()),
                Owner::Operands(closing.clone()),
            ]
            .into(),
        );
        event.point.site == *closing
            && event.point.phase == Phase::Leave
            && *closing != source.site
            && step.selected == ExitKind::Normal
            && step.normal == ExitKind::Normal
            && event.output.is_none()
            && event.inputs.is_empty()
            && event.entities.is_empty()
            && event.peak.fixed == 0
            && event.peak.terms.is_empty()
            && event.delta.fixed == 0
            && matches!(event.delta.terms.as_slice(), [(measure, -1)] if *measure == release)
            && !publication.borrowed_inputs
            && publication.restored_roles.is_none()
            && publication.assignment.is_none()
            && !publication.member
            && publication.field.is_none()
            && !source
                .room
                .scratch
                .iter()
                .any(|lease| lease.at == event.point)
    }
    /// A local binding's Commit is a proven move when its one event moves the
    /// RHS operand into its output owner, at most releasing that owner's
    /// previous value: no entity, peak, scratch lease, cell or member edit,
    /// and both exits normal. Returns the output selector.
    pub(crate) fn binding_move(&self, source: &Source, rhs: &Site) -> Option<&Owner> {
        let ([Selection::Event { .. }], [step]) = (self.route.as_ref(), self.steps.as_ref()) else {
            return None;
        };
        let event = &step.event;
        let publication = &step.publication;
        let output = event.output.as_ref()?;
        let releases_output = || {
            event.delta.terms.len() == 1
                && event.delta.terms.iter().all(|(measure, coefficient)| {
                    *coefficient == -1
                        && matches!(measure, Measure::Release(group) if **group == [output.clone()])
                })
        };
        (event.point.phase == Phase::Commit
            && step.selected == ExitKind::Normal
            && step.normal == ExitKind::Normal
            && *event.inputs == [Owner::Operand(rhs.clone())]
            && event.entities.is_empty()
            && event.peak.fixed == 0
            && event.peak.terms.is_empty()
            && event.delta.fixed == 0
            && (event.delta.terms.is_empty() || releases_output())
            && !publication.borrowed_inputs
            && publication.restored_roles.is_none()
            && publication
                .assignment
                .as_ref()
                .is_none_or(|_| !source.assigns_cell(&event.point.site))
            && !publication.member
            && publication.field.is_none()
            && !source
                .room
                .scratch
                .iter()
                .any(|lease| lease.at == event.point))
        .then_some(output)
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
    for measure in event.peak.measures() {
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
