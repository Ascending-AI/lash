//! Evaluate the source control tree, never a fast path's instruction schedule.
use super::*;

pub(super) fn prefix(
    source: &Source,
    at: &Site,
    execution: &mut impl Execution,
    budget: usize,
) -> Result<CheckedRoom, Refused> {
    if at.unit != source.site.unit || !at.path.starts_with(&source.site.path) {
        return Err(Refused::Path);
    }
    let mut node = doc::Node::Block(&source.body);
    for index in &at.path[source.site.path.len()..] {
        node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
    }
    let mut run = Run {
        source,
        execution,
        budget,
        balance: 0,
        peak: 0,
    };
    let exit = run.node(node, at)?;
    Ok(CheckedRoom {
        owner: source.owner,
        native: false,
        whole: at == &source.site,
        native_contract: None,
        balance: Balance {
            peak_extra: u64::try_from(run.peak.max(0)).map_err(|_| Refused::Facts)?,
            retained_delta: run.balance,
        },
        exit,
    })
}
struct Run<'a, E> {
    source: &'a Source,
    execution: &'a mut E,
    budget: usize,
    balance: i128,
    peak: i128,
}
impl<E: Execution> Run<'_, E> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        self.budget = self.budget.checked_sub(1).ok_or(Refused::Path)?;
        let at = point(site, phase);
        let e = self.source.room.peak_extra.get(&at).ok_or(Refused::Path)?;
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
        let delta = self.source.room.retained_delta[&at]
            .delta
            .evaluate(facts.as_ref())
            .ok_or(Refused::Facts)?;
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
        for m in e.peak.terms.keys() {
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
    fn child(
        &mut self,
        node: doc::Node<'_>,
        site: &Site,
        index: usize,
    ) -> Result<ExitKind, Refused> {
        self.node(
            *node.children().get(index).ok_or(Refused::Path)?,
            &child(site, index),
        )
    }
    fn node(&mut self, node: doc::Node<'_>, site: &Site) -> Result<ExitKind, Refused> {
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
            self.step(site, Phase::Leave)?;
            return Ok(exit);
        }
        match node {
            doc::Node::Expr(doc::Expr::If { .. }) | doc::Node::Stmt(doc::Stmt::If { .. }) => {
                let exit = self.child(node, site, 0)?;
                if exit != ExitKind::Normal {
                    return Ok(exit);
                }
                self.step(site, Phase::Test)?;
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
                self.step(site, Phase::Test)?;
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
                    self.step(site, Phase::Test)?;
                    if !self.choose(site)? {
                        break;
                    }
                    match self.child(node, site, 1)? {
                        ExitKind::Normal | ExitKind::Continue => {}
                        ExitKind::Break => {
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
                if via.is_some() {
                    let exit = self.step(site, Phase::Open)?;
                    if exit != ExitKind::Normal {
                        return Ok(exit);
                    }
                }
                loop {
                    if via.as_ref().is_some_and(|v| v.next.is_some()) {
                        let exit = self.step(site, Phase::Next)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                    }
                    if !self.choose(site)? {
                        break;
                    }
                    if via.as_ref().is_some_and(|v| v.check.is_some()) {
                        let exit = self.step(site, Phase::Check)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                    }
                    if via.as_ref().is_some_and(|v| v.item.is_some()) {
                        let exit = self.step(site, Phase::Item)?;
                        if exit != ExitKind::Normal {
                            return Ok(exit);
                        }
                    }
                    self.step(site, Phase::Test)?;
                    match self.child(node, site, 1)? {
                        ExitKind::Normal | ExitKind::Continue => {}
                        ExitKind::Break => {
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
                    self.step(site, Phase::Catch)?;
                    exit = self.child(node, site, 1)?;
                }
                if exit == ExitKind::Action {
                    return Ok(exit);
                }
                if t.finally.is_some() {
                    let cleanup = self.child(node, site, if t.catch.is_some() { 2 } else { 1 })?;
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
            return self.step(site, Phase::Enter);
        }
        self.step(site, Phase::Commit)
    }
}
