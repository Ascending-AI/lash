//! A caller route with explicit, source-authenticated callee return edges.
//! This product cannot be passed to ordinary interval publication/evaluation.
use super::*;
use crate::compile::room::region::calls::{CallCompletion, VerifiedCall};
use crate::compile::{CodeId, Executable, StmtId};

pub(crate) struct ComposedInterval {
    source: Arc<Source>,
    executable: u64,
    code: CodeId,
    route: Box<[Selection]>,
    events: Box<[Event]>,
    calls: Box<[CallCompletion]>,
}
impl ComposedInterval {
    pub(crate) fn selections(&self) -> &[Selection] {
        &self.route
    }
    pub(crate) fn events(&self) -> impl Iterator<Item = &Event> {
        self.events.iter()
    }
    pub(crate) fn calls(&self) -> &[CallCompletion] {
        &self.calls
    }
    pub(in crate::compile::room) fn matches(
        &self,
        exe: &Executable,
        code: CodeId,
        calls: &[Arc<VerifiedCall>],
    ) -> bool {
        self.executable == exe.local()
            && self.code == code
            && Arc::ptr_eq(&self.source, &exe.code(code).source_room)
            && calls.len() == self.calls.len()
            && calls.iter().all(|call| {
                self.calls
                    .iter()
                    .any(|boundary| Arc::ptr_eq(boundary.call(), call))
            })
    }

    /// Only a slice of the already-authenticated caller route is reduced.
    /// A callee entry or resume must be consumed together by its Call piece.
    pub(in crate::compile::room) fn scalar(
        &self,
        start: usize,
        end: usize,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        let selected = self.route.get(start..end).ok_or(Refused::Path)?;
        let mut variables = 0;
        let mut steps = Vec::new();
        for selection in selected {
            let Selection::Event { at, exit } = selection else {
                continue;
            };
            if self
                .calls
                .iter()
                .any(|call| call.entry().0 == at || call.resume() == at)
            {
                return Err(Refused::Source);
            }
            let event = self.source.room.events.get(at).ok_or(Refused::Path)?;
            steps.push(Step::compile(
                &self.source,
                event.clone(),
                *exit,
                &mut variables,
            )?);
        }
        let at = steps.first().ok_or(Refused::Path)?.event.point.site.clone();
        let exit = steps.last().ok_or(Refused::Path)?.selected;
        Ok(Arc::new(CompiledInterval {
            owner: self.source.owner,
            source: self.source.site.clone(),
            at,
            route: selected.into(),
            steps: steps.into(),
            variables,
            exit,
            whole: false,
            return_loops: None,
            resume: None,
            acquires: OnceLock::new(),
            commits: OnceLock::new(),
        }))
    }
}

struct Route<'a> {
    source: &'a Arc<Source>,
    calls: &'a [Arc<VerifiedCall>],
    selected: Vec<Selection>,
    through: &'a Point,
    branch: &'a Site,
    stop: bool,
}
impl evaluate::Events for Route<'_> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let at = point(site, phase);
        let exit = self
            .source
            .room
            .exits
            .iter()
            .find(|exit| exit.point == at)
            .ok_or(Refused::Exit)?
            .kind;
        self.stop = at == *self.through;
        self.selected.push(Selection::Event { at, exit });
        Ok(exit)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        if site != self.branch {
            return Err(Refused::Path);
        }
        self.selected.push(Selection::Branch {
            at: site.clone(),
            taken: true,
        });
        Ok(true)
    }
    fn selected_end(&self) -> bool {
        self.stop
    }
    fn resume_call(
        &mut self,
        source: &Source,
        at: &Point,
        exit: ExitKind,
    ) -> Result<Option<CallCompletion>, Refused> {
        completion(self.calls, source, at, exit)
    }
}
struct ComposedTrace<'a> {
    trace: Trace<'a>,
    allowed: &'a [Arc<VerifiedCall>],
    completed: Vec<CallCompletion>,
}
impl evaluate::Events for ComposedTrace<'_> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        self.trace.step(site, phase)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        self.trace.choose(site)
    }
    fn selected_end(&self) -> bool {
        self.trace.selected_end()
    }
    fn resume_call(
        &mut self,
        source: &Source,
        at: &Point,
        exit: ExitKind,
    ) -> Result<Option<CallCompletion>, Refused> {
        let completed = completion(self.allowed, source, at, exit)?;
        if let Some(ref call) = completed {
            if self
                .completed
                .iter()
                .any(|prior| Arc::ptr_eq(prior.call(), call.call()))
            {
                return Err(Refused::Source);
            }
            self.completed
                .push(completion(self.allowed, source, at, exit)?.ok_or(Refused::Source)?);
        }
        Ok(completed)
    }
}
fn completion(
    calls: &[Arc<VerifiedCall>],
    source: &Source,
    at: &Point,
    exit: ExitKind,
) -> Result<Option<CallCompletion>, Refused> {
    calls
        .iter()
        .find(|call| call.at() == at)
        .map(|call| call.completion(source, at, exit))
        .transpose()
}

impl Source {
    pub(crate) fn compile_composed_loop(
        self: &Arc<Self>,
        exe: &Executable,
        code: CodeId,
        statement: StmtId,
        calls: &[Arc<VerifiedCall>],
    ) -> Result<Arc<ComposedInterval>, Refused> {
        let stmt = exe.stmt(statement);
        if exe.stmt_at(code, &stmt.site) != Some(statement)
            || !matches!(stmt.form, crate::compile::Form::While { .. })
        {
            return Err(Refused::Source);
        }
        verify_calls(self, exe, code, calls)?;
        let through = point(&stmt.site.child(1), Phase::Leave);
        let mut route = Route {
            source: self,
            calls,
            selected: vec![],
            through: &through,
            branch: &stmt.site,
            stop: false,
        };
        evaluate::walk_to(self, &stmt.site, &mut route, 4096)?;
        let first = self.first_event(&stmt.site)?;
        self.compile_composed_between(exe, code, &first, &through, &route.selected, calls)
    }

    pub(crate) fn compile_composed_between(
        self: &Arc<Self>,
        exe: &Executable,
        code: CodeId,
        from: &Point,
        through: &Point,
        selected: &[Selection],
        calls: &[Arc<VerifiedCall>],
    ) -> Result<Arc<ComposedInterval>, Refused> {
        verify_calls(self, exe, code, calls)?;
        if selected.len() > 4096
            || calls.is_empty()
            || !matches!(selected.first(), Some(Selection::Event { at, .. }) if at == from)
            || !matches!(selected.last(), Some(Selection::Event { at, .. }) if at == through)
        {
            return Err(Refused::Path);
        }
        let mut context = from.site.clone();
        context.path.truncate(
            from.site
                .path
                .iter()
                .zip(&through.site.path)
                .take_while(|(a, b)| a == b)
                .count(),
        );
        let mut trace = ComposedTrace {
            trace: Trace {
                source: self,
                selected,
                cursor: 0,
                events: vec![],
                stop: true,
            },
            allowed: calls,
            completed: vec![],
        };
        let exit = evaluate::walk_to(self, &context, &mut trace, 4096)?;
        if trace.trace.cursor != selected.len()
            || trace.completed.len() != calls.len()
            || exit != ExitKind::Normal
        {
            return Err(Refused::Path);
        }
        Ok(Arc::new(ComposedInterval {
            source: Arc::clone(self),
            executable: exe.local(),
            code,
            route: selected.into(),
            events: trace
                .trace
                .events
                .into_iter()
                .map(|(event, _)| event)
                .collect(),
            calls: trace.completed.into(),
        }))
    }
}
fn verify_calls(
    source: &Arc<Source>,
    exe: &Executable,
    code: CodeId,
    calls: &[Arc<VerifiedCall>],
) -> Result<(), Refused> {
    source.verify()?;
    if !Arc::ptr_eq(source, &exe.code(code).source_room) {
        return Err(Refused::Source);
    }
    for (index, call) in calls.iter().enumerate() {
        call.verify(exe)?;
        if !call.belongs_to(source, code)
            || calls[..index].iter().any(|prior| prior.at() == call.at())
        {
            return Err(Refused::Source);
        }
    }
    Ok(())
}
