//! Source control is walked only while forming a checked path. Runtime interval
//! evaluation uses cached ledger algebra, independently of executor scheduling.
use super::*;

pub(super) trait Events {
    /// Navigation may skip a completed source subtree. Pricing and interval
    /// verification use the default and always visit their selected events.
    fn completed(&mut self, _: &Site) -> Option<ExitKind> {
        None
    }
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused>;
    fn choose(&mut self, site: &Site) -> Result<bool, Refused>;
    /// What the `for … via` at `site` walks, from what its `open` gave. A
    /// route that cannot know it cannot know which hooks run.
    fn walked(&mut self, _: &Site) -> Result<Walk, Refused> {
        Err(Refused::Facts)
    }
    fn selected_end(&self) -> bool {
        false
    }
    /// A selected suffix that starts where a delivered action or a call
    /// resumes: the point it resumes at after the event at `at`.
    fn resumed(&mut self, _: &Point) -> Option<Point> {
        None
    }
}

enum Stop {
    Refused(Refused),
    Selected(ExitKind),
}
impl From<Refused> for Stop {
    fn from(refused: Refused) -> Self {
        Self::Refused(refused)
    }
}

pub(super) fn walk(
    source: &Source,
    at: &Site,
    events: &mut impl Events,
    budget: usize,
) -> Result<ExitKind, Refused> {
    match run(source, at, events, budget) {
        Ok(exit) => Ok(exit),
        Err(Stop::Refused(refused)) => Err(refused),
        Err(Stop::Selected(_)) => Err(Refused::Path),
    }
}

/// Validate the ordinary source path until its selected event boundary.
/// An internal stop is distinct from a guest return, raise or suspension.
pub(super) fn walk_to(
    source: &Source,
    at: &Site,
    events: &mut impl Events,
    budget: usize,
) -> Result<ExitKind, Refused> {
    match run(source, at, events, budget) {
        Err(Stop::Selected(exit)) => Ok(exit),
        Err(Stop::Refused(refused)) => Err(refused),
        Ok(_) => Err(Refused::Path),
    }
}
fn run(
    source: &Source,
    at: &Site,
    events: &mut impl Events,
    budget: usize,
) -> Result<ExitKind, Stop> {
    if at.unit != source.site.unit || !at.path.starts_with(&source.site.path) {
        return Err(Refused::Path.into());
    }
    let mut node = doc::Node::Block(&source.body);
    for index in &at.path[source.site.path.len()..] {
        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
    }
    Run {
        source,
        events,
        budget,
    }
    .node(node, at)
}

#[cfg(test)]
pub(super) fn prefix(
    source: &Source,
    at: &Site,
    execution: &mut impl Execution,
    budget: usize,
) -> Result<CheckedRoom, Refused> {
    let mut prices = Prices {
        source,
        execution,
        balance: 0,
        peak: 0,
    };
    let exit = walk(source, at, &mut prices, budget)?;
    Ok(CheckedRoom {
        owner: source.owner,
        native: false,
        whole: at == &source.site,
        native_contract: None,
        balance: Balance {
            peak_extra: u64::try_from(prices.peak.max(0)).map_err(|_| Refused::Facts)?,
            retained_delta: prices.balance,
        },
        exit,
    })
}
#[cfg(test)]
struct Prices<'a, E> {
    source: &'a Source,
    execution: &'a mut E,
    balance: i128,
    peak: i128,
}
#[cfg(test)]
impl<E: Execution> Events for Prices<'_, E> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let at = point(site, phase);
        let e = self.source.room.events.get(&at).ok_or(Refused::Path)?;
        let facts = self.execution.event(&at).ok_or(Refused::Facts)?;
        for scratch in self.source.room.scratch.iter().filter(|s| s.at == at) {
            scratch
                .amount
                .evaluate(facts.as_ref())
                .ok_or(Refused::Scratch)?;
        }
        let extra = e.peak.evaluate(facts.as_ref()).ok_or(Refused::Facts)?;
        if extra < 0 {
            return Err(Refused::Facts);
        }
        self.peak = self
            .peak
            .max(self.balance.checked_add(extra).ok_or(Refused::Facts)?);
        let delta = e.delta.evaluate(facts.as_ref()).ok_or(Refused::Facts)?;
        self.balance = self.balance.checked_add(delta).ok_or(Refused::Facts)?;
        self.peak = self.peak.max(self.balance);
        if facts.raised() {
            if e.output.is_none() {
                return Err(Refused::Facts);
            }
            if self
                .source
                .room
                .exits
                .iter()
                .any(|e| e.point == at && e.kind == ExitKind::Raise)
            {
                return Ok(ExitKind::Raise);
            }
            return Err(Refused::Exit);
        }
        for m in e.peak.measures() {
            if let Measure::CallPeak { at, callee } = m {
                return Ok(match facts.call(at, *callee).ok_or(Refused::Facts)?.exit {
                    ExitKind::Raise => ExitKind::Raise,
                    _ => ExitKind::Normal,
                });
            }
        }
        self.source
            .room
            .exits
            .iter()
            .find(|e| e.point == at)
            .map(|e| e.kind)
            .ok_or(Refused::Exit)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        self.execution.choose(site).ok_or(Refused::Facts)
    }
    /// No source execution names what a `for … via` walks.
    fn walked(&mut self, _: &Site) -> Result<Walk, Refused> {
        Err(Refused::Facts)
    }
}
struct Run<'a, E> {
    source: &'a Source,
    events: &'a mut E,
    budget: usize,
}
impl<E: Events> Run<'_, E> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Stop> {
        self.step_departure(site, phase, None)
    }
    fn step_departure(
        &mut self,
        site: &Site,
        phase: Phase,
        departure: Option<ExitKind>,
    ) -> Result<ExitKind, Stop> {
        self.budget = self.budget.checked_sub(1).ok_or(Refused::Path)?;
        let exit = self.events.step(site, phase)?;
        if self.events.selected_end() {
            return Err(Stop::Selected(departure.unwrap_or(exit)));
        }
        Ok(exit)
    }
    fn call(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Stop> {
        let exit = self.step(site, phase)?;
        if let Some(resume) = self.events.resumed(&point(site, phase)) {
            if resume.site != *site {
                return Err(Refused::Source.into());
            }
            return self.step(&resume.site, resume.phase);
        }
        Ok(exit)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Stop> {
        self.events.choose(site).map_err(Stop::Refused)
    }
    fn child(&mut self, node: doc::Node<'_>, site: &Site, index: usize) -> Result<ExitKind, Stop> {
        self.node(
            *node.children().get(index).ok_or(Refused::Path)?,
            &child(site, index),
        )
    }
    fn node(&mut self, node: doc::Node<'_>, site: &Site) -> Result<ExitKind, Stop> {
        if let Some(exit) = self.events.completed(site) {
            return Ok(exit);
        }
        if matches!(node, doc::Node::Block(_)) {
            self.step(site, Phase::Enter)?;
            let mut exit = ExitKind::Normal;
            for (i, c) in node.children().into_iter().enumerate() {
                exit = self.node(c, &child(site, i))?;
                if exit != ExitKind::Normal {
                    break;
                }
            }
            // A suspension preserves all ordinary source holdings. Cleanup is
            // performed by the resumed source suffix, not by a scratch lease.
            if exit == ExitKind::Action {
                return Ok(exit);
            }
            if exit == ExitKind::Normal && *site == self.source.site {
                self.step(site, Phase::Fallthrough)?;
            }
            self.step_departure(site, Phase::Leave, Some(exit))?;
            return Ok(exit);
        }
        if matches!(node, doc::Node::Expr(doc::Expr::Act(act)) if matches!(act.kind, doc::ActKind::Call(doc::Callee::Value(_)) | doc::ActKind::Spawn(doc::Callee::Value(_))))
        {
            let exit = self.step(site, Phase::Open)?;
            if exit != ExitKind::Normal {
                return Ok(exit);
            }
        }
        match node {
            doc::Node::Expr(doc::Expr::If { .. }) | doc::Node::Stmt(doc::Stmt::If { .. }) => {
                let exit = self.child(node, site, 0)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
                let exit = self.step(site, Phase::Test)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
                let arm = if self.choose(site)? { 1 } else { 2 };
                let exit = self.child(node, site, arm)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
            }
            doc::Node::Expr(doc::Expr::Let { .. }) => {
                let exit = self.child(node, site, 0)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
                let exit = self.step(site, Phase::Test)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
                let exit = self.child(node, site, 1)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
            }
            doc::Node::Stmt(doc::Stmt::While { else_block, .. }) => {
                let mut broken = false;
                loop {
                    let exit = self.child(node, site, 0)?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                    let exit = self.step(site, Phase::Test)?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                    if !self.choose(site)? {
                        break;
                    }
                    match self.child(node, site, 1)? {
                        ExitKind::Normal => {}
                        // A selected Catch ends a suffix carrying its departure.
                        ExitKind::Continue => {
                            self.step_departure(site, Phase::Catch, Some(ExitKind::Continue))?;
                        }
                        ExitKind::Break => {
                            self.step_departure(site, Phase::Catch, Some(ExitKind::Break))?;
                            broken = true;
                            break;
                        }
                        exit => return Ok(exit),
                    }
                }
                if !broken && !else_block.is_empty() {
                    let exit = self.child(node, site, 2)?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                }
            }
            doc::Node::Stmt(doc::Stmt::For {
                else_block, via, ..
            }) => {
                let exit = self.child(node, site, 0)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
                let mut broken = false;
                // K-ITER-005: `open` takes the value to what the loop walks;
                // each pass then steps a driver through `next`, any other
                // walk by its own cursor, and a checked map or set runs
                // `check` after each step, at exhaustion too.
                let walk = match via {
                    Some(_) => {
                        let exit = self.step(site, Phase::Open)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                        Some(self.events.walked(site)?)
                    }
                    None => {
                        let exit = self.step(site, Phase::Enter)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                        None
                    }
                };
                loop {
                    if walk == Some(Walk::Driver) {
                        let exit = self.step(site, Phase::Next)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                    }
                    let more = self.choose(site)?;
                    if walk == Some(Walk::Guarded) {
                        let exit = self.step(site, Phase::Check)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                    }
                    if !more {
                        break;
                    }
                    if via.as_ref().is_some_and(|v| v.item.is_some()) {
                        let exit = self.step(site, Phase::Item)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                    }
                    let exit = self.step(site, Phase::Test)?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                    match self.child(node, site, 1)? {
                        ExitKind::Normal => {}
                        // A selected Catch ends a suffix carrying its departure.
                        ExitKind::Continue => {
                            self.step_departure(site, Phase::Catch, Some(ExitKind::Continue))?;
                        }
                        ExitKind::Break => {
                            self.step_departure(site, Phase::Catch, Some(ExitKind::Break))?;
                            broken = true;
                            break;
                        }
                        exit => return Ok(exit),
                    }
                }
                if !broken && !else_block.is_empty() {
                    let exit = self.child(node, site, 2)?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                }
            }
            doc::Node::Stmt(doc::Stmt::Try(t)) => {
                let mut exit = self.child(node, site, 0)?;
                if exit == ExitKind::Action {
                    return Ok(exit);
                }
                if exit == ExitKind::Raise && t.catch.is_some() {
                    // A selected Catch ends a suffix carrying its raise.
                    self.step_departure(site, Phase::Catch, Some(ExitKind::Raise))?;
                    exit = self.child(node, site, 1)?;
                }
                if exit == ExitKind::Action {
                    return Ok(exit);
                }
                if t.finally.is_some() {
                    let suspended = exit != ExitKind::Normal;
                    // A selected Suspend ends a suffix carrying its departure.
                    if suspended {
                        self.step_departure(site, Phase::Suspend, Some(exit))?;
                    }
                    let cleanup = self.child(node, site, if t.catch.is_some() { 2 } else { 1 })?;
                    if cleanup == ExitKind::Action {
                        return Ok(cleanup);
                    }
                    if suspended {
                        self.step_departure(
                            site,
                            if cleanup == ExitKind::Normal {
                                Phase::Restore
                            } else {
                                Phase::Supersede
                            },
                            Some(if cleanup == ExitKind::Normal {
                                exit
                            } else {
                                cleanup
                            }),
                        )?;
                    }
                    if cleanup != ExitKind::Normal {
                        exit = cleanup;
                    }
                }
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
            }
            doc::Node::Stmt(doc::Stmt::Assign {
                place: doc::Place::Member(_),
                order: doc::AssignOrder::ValueFirst,
                ..
            }) => {
                let children = node.children();
                let last = children.len() - 1;
                let exit = self.child(node, site, last)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
                for (i, c) in children[..last].iter().enumerate() {
                    let exit = self.node(*c, &child(site, i))?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                }
            }
            _ => {
                for (i, c) in node.children().into_iter().enumerate() {
                    let exit = self.node(c, &child(site, i))?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                }
            }
        }
        if matches!(
            node,
            doc::Node::Action(_) | doc::Node::Expr(doc::Expr::Act(_))
        ) {
            return self.call(site, Phase::Enter);
        }
        if matches!(node, doc::Node::Expr(doc::Expr::Call { .. })) {
            self.call(site, Phase::Commit)
        } else {
            self.step(site, Phase::Commit)
        }
    }
}
