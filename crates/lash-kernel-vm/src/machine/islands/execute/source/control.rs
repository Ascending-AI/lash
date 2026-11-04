//! Break and continue publish their real entered suffix before physical unwind.
use super::*;
use room::{ExitKind, Phase as SourcePhase, Point};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_control_departure(
        &mut self,
        frame: &mut Values<'_, '_>,
        continuing: bool,
    ) -> Result<bool, HeapFault> {
        if !self.source_departure_frame(frame) || self.projection().is_some() {
            return Ok(false);
        }
        let Some(id) = frame.state.statement else {
            return Ok(false);
        };
        let statement = self.exe.stmt(id);
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let Ok(interval) = source.compile_departure_suffix(&Point {
            site: statement.site.clone(),
            phase: SourcePhase::Commit,
        }) else {
            return Ok(false);
        };
        let kind = if continuing {
            ExitKind::Continue
        } else {
            ExitKind::Break
        };
        if interval.exit() != kind {
            return Err(malformed(
                "packet control departure has another source exit",
            ));
        }
        let Some(last) = interval.events().last() else {
            return Ok(false);
        };
        let suspended = last.point.phase == SourcePhase::Suspend;
        let caught = last.point.phase == SourcePhase::Catch;
        if !suspended && !caught {
            return Ok(false);
        }
        #[cfg(test)]
        if (suspended && crate::machine::islands::witness::suspended_departure_declined())
            || (caught && crate::machine::islands::witness::caught_departure_declined())
        {
            return Ok(false);
        }
        let target = |control: &Position| match control {
            Position::Try {
                cleanup,
                phase: Phase::Body | Phase::Catch,
            } => suspended && self.exe.stmt(cleanup.id).site == last.point.site,
            Position::Loop { id, .. } => caught && self.exe.stmt(*id).site == last.point.site,
            _ => false,
        };
        let Some(entered) = frame
            .state
            .control
            .iter()
            .rev()
            .find(|control| target(control))
        else {
            return Err(malformed("packet control departure has no entered target"));
        };
        if !interval
            .events()
            .skip(1)
            .filter(|event| event.point.phase == SourcePhase::Leave)
            .map(|event| &event.point.site)
            .eq(frame
                .state
                .control
                .iter()
                .rev()
                .take_while(|control| !target(control))
                .filter_map(|control| match control {
                    Position::Block(block, _) => Some(
                        &self
                            .exe
                            .block(frame.state.callee.compiled.blocks[*block].id)
                            .site,
                    ),
                    _ => None,
                }))
        {
            return Err(malformed(
                "packet control departure differs from its entered blocks",
            ));
        }
        let cursor = match entered {
            Position::Loop {
                id,
                cursor: Some(cursor),
                guard: None,
                ..
            } if continuing => {
                let crate::compile::Form::For {
                    site, via: None, ..
                } = &self.exe.stmt(*id).form
                else {
                    return Ok(false);
                };
                let Some(Ok((_, next))) = cursor.resident_step(self.arena) else {
                    return Ok(false);
                };
                let Ok(head) = source.next_after_for_cursor(site, next.is_some()) else {
                    return Ok(false);
                };
                Some((head, cursor.walked().and_then(|value| value.handle())))
            }
            Position::Loop {
                cursor: Some(_), ..
            } if continuing => return Ok(false),
            _ => None,
        };
        let loops = self.source_loops(&frame.state.control);
        let Some(activation) = frame.state.source.as_mut() else {
            return Ok(false);
        };
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let store = self.logical.clone();
        let ledger = store.borrow();
        let Some(recipe) = (crate::machine::source_publication::return_storage::ReturnInput {
            source,
            invocation: &activation.invocation,
            task: self.task,
            at: &statement.site,
            carried: None,
            safe_point: self.charged,
            loops,
            cursor,
        })
        .prepare(&ledger, self.arena, self.heap)?
        else {
            return Ok(false);
        };
        drop(ledger);
        self.publish_source_departure(frame.state.callee, activation, recipe)
    }
}
