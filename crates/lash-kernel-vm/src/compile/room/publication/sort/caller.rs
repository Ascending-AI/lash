//! Original caller events and the verified return edge stay in one route.
use super::*;
use crate::compile::room::{
    evaluate,
    interval::ComposedInterval,
    region::calls::{CallCompletion, VerifiedCall},
};

struct Route<'a> {
    source: &'a Arc<Source>,
    calls: &'a [Arc<VerifiedCall>],
    first: &'a Point,
    through: &'a Point,
    choices: &'a [(Site, bool)],
    selected: Vec<Selection>,
    started: bool,
    finished: bool,
}
impl evaluate::Events for Route<'_> {
    fn completed(&mut self, site: &Site) -> Option<ExitKind> {
        (!self.started && !self.first.site.path.starts_with(&site.path)).then_some(ExitKind::Normal)
    }
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let at = Point {
            site: site.clone(),
            phase,
        };
        if !self.started {
            if at != *self.first {
                return Ok(ExitKind::Normal);
            }
            self.started = true;
        }
        let exit = self
            .source
            .room
            .exits
            .iter()
            .find(|exit| exit.point == at)
            .ok_or(Refused::Exit)?
            .kind;
        self.finished = at == *self.through;
        self.selected.push(Selection::Event { at, exit });
        Ok(exit)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        if !self.started {
            return match self.first.site.path.get(site.path.len()) {
                Some(1) => Ok(true),
                Some(2) => Ok(false),
                _ => Err(Refused::Path),
            };
        }
        let taken = self
            .choices
            .iter()
            .find(|(at, _)| at == site)
            .ok_or(Refused::Path)?
            .1;
        self.selected.push(Selection::Branch {
            at: site.clone(),
            taken,
        });
        Ok(taken)
    }
    fn resume_call(
        &mut self,
        source: &Source,
        at: &Point,
        exit: ExitKind,
    ) -> Result<Option<CallCompletion>, Refused> {
        if !self.started {
            return Ok(None);
        }
        self.calls
            .iter()
            .find(|call| call.at() == at)
            .map(|call| call.completion(source, at, exit))
            .transpose()
    }
    fn selected_end(&self) -> bool {
        self.finished
    }
}

/// The actual selected route also verifies ordinary statement schedules.
/// Structural source events may separate closes without creating a fee.
pub(in crate::compile::room) fn source_route(
    exe: &Executable,
    code: CodeId,
    first: Point,
    through: Point,
    choices: &[(Site, bool)],
    calls: &[Arc<VerifiedCall>],
) -> Result<Arc<ComposedInterval>, Refused> {
    let source = &exe.code(code).source_room;
    let mut context = first.site.clone();
    context.path.truncate(
        first
            .site
            .path
            .iter()
            .zip(&through.site.path)
            .take_while(|(a, b)| a == b)
            .count(),
    );
    let mut route = Route {
        source,
        calls,
        first: &first,
        through: &through,
        choices,
        selected: Vec::new(),
        started: false,
        finished: false,
    };
    if evaluate::walk_to(source, &context, &mut route, 4096)? != ExitKind::Normal || !route.finished
    {
        return Err(Refused::Path);
    }
    let branches: Vec<_> = route
        .selected
        .iter()
        .filter_map(|selection| match selection {
            Selection::Branch { at, taken } => Some((at.clone(), *taken)),
            _ => None,
        })
        .collect();
    if branches != choices {
        return Err(Refused::Source);
    }
    source.compile_composed_between(exe, code, &first, &through, &route.selected, calls)
}
