//! Only a verified pure call can compile this caller-side continuation. The
//! callee keeps its own Source and logical invocation throughout composition.
use super::*;

impl Source {
    pub(in crate::compile::room) fn compile_call_continuation(
        &self,
        call: &Point,
        through: &Point,
        selected: &[Selection],
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        self.compile_event(
            &Point {
                site: call.site.clone(),
                phase: Phase::Restore,
            },
            ExitKind::Normal,
        )?;
        let from = self.next_after_commit(&call.site)?;
        if selected.is_empty()
            || !matches!(selected.first(), Some(Selection::Event { at, .. }) if *at == from)
            || !matches!(selected.last(), Some(Selection::Event { at, .. }) if at == through)
            || !call.site.path.starts_with(&through.site.path)
        {
            return Err(Refused::Source);
        }
        let key = Key {
            event: false,
            call: true,
            join: Some(call.site.clone()),
            at: from.site.clone(),
            bounds: Some((from.clone(), through.clone())),
            selected: selected.into(),
        };
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = cache.get(&key) {
            return Ok(Arc::clone(interval));
        }
        struct After<'a> {
            call: &'a Site,
            trace: Trace<'a>,
        }
        impl evaluate::Events for After<'_> {
            fn completed(&mut self, site: &Site) -> Option<ExitKind> {
                if site == self.call
                    || (!self.call.path.starts_with(&site.path) && site.path < self.call.path)
                {
                    Some(ExitKind::Normal)
                } else {
                    None
                }
            }
            fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
                self.trace.step(site, phase)
            }
            fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
                self.trace.choose(site)
            }
            fn selected_end(&self) -> bool {
                self.trace.selected_end()
            }
        }
        let mut after = After {
            call: &call.site,
            trace: Trace {
                source: self,
                selected,
                cursor: 0,
                events: vec![],
                stop: true,
            },
        };
        let exit = evaluate::walk_to(
            self,
            &through.site,
            &mut after,
            selected.len() + call.site.path.len() + 4,
        )?;
        if after.trace.cursor != selected.len() {
            return Err(Refused::Path);
        }
        let mut variables = 0;
        let steps = after
            .trace
            .events
            .into_iter()
            .map(|(event, exit)| Step::compile(self, event, exit, &mut variables))
            .collect::<Result<Box<[_]>, _>>()?;
        let interval = Arc::new(CompiledInterval {
            owner: self.owner,
            source: self.site.clone(),
            at: from.site,
            route: selected.into(),
            steps,
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
}
