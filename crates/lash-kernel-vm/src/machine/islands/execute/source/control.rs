//! Break and continue publish their real entered suffix before physical unwind.
use super::*;
use crate::admission::Allowance;
use crate::machine::source_publication::Prefix;
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{ExactRoots, Successor},
};

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
        // Iterator-helper loops retain a distinct source iterable operand.
        // Their precise path must perform that Commit; a cursor alone cannot
        // stand in for the original operand at packet completion.
        if caught
            && !continuing
            && let Position::Loop { id, .. } = entered
            && matches!(
                self.exe.stmt(*id).form,
                crate::compile::Form::For { via: Some(_), .. }
            )
        {
            return Ok(false);
        }
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
        if !self.publish_source_departure(frame.state.callee, activation, recipe)? {
            return Ok(false);
        }
        if caught && !continuing {
            // Catch consumes the departure, but leaves the loop's Commit
            // unpaid. Publish its source release before physical unwind drops
            // the cursor that still owns the iterable.
            self.publish_broken_loop_commit(frame, &last.point.site)?;
        }
        Ok(true)
    }

    fn publish_broken_loop_commit(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &lash_kernel_value::sites::Site,
    ) -> Result<(), HeapFault> {
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: site.clone(),
            phase: SourcePhase::Commit,
        };
        let mut parents = frame.state.control.iter().rev().skip_while(|control| {
            !matches!(control, Position::Loop { id, .. } if self.exe.stmt(*id).site == *site)
        });
        let Some(Position::Loop { id, .. }) = parents.next() else {
            return Err(malformed("caught break lost its loop"));
        };
        let is_for = matches!(self.exe.stmt(*id).form, crate::compile::Form::For { .. });
        let Some(Position::Block(block, next)) = parents.next() else {
            return Err(malformed("caught loop has no parent block"));
        };
        let parent = self
            .exe
            .block(frame.state.callee.compiled.blocks[*block].id);
        let unpaid = if let Some(id) = parent.stmts.get(*next) {
            source
                .first_event(&self.exe.stmt(*id).site)
                .map_err(|_| malformed("caught loop successor failed verification"))?
        } else {
            Point {
                site: parent.site.clone(),
                phase: if parent.site == source.site {
                    SourcePhase::Fallthrough
                } else {
                    SourcePhase::Leave
                },
            }
        };
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("caught break lost its source activation"))?;
        let before = activation
            .frontier
            .as_ref()
            .ok_or_else(|| malformed("caught break lost its source frontier"))?
            .position();
        if before.at
            != (Point {
                site: site.clone(),
                phase: SourcePhase::Catch,
            })
            || before.unpaid.as_ref() != Some(&at)
        {
            return Err(malformed("caught break has another unpaid source event"));
        }
        let start = room::publication::Position {
            at: at.clone(),
            ..before.clone()
        };
        let next = room::publication::Position {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let interval = source
            .compile_event(&at, ExitKind::Normal)
            .map_err(|_| malformed("caught loop Commit failed verification"))?;
        let groups = vec![room::Owner::Operands(site.clone())];
        let store = self.logical.clone();
        let ledger = store.borrow();
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("caught loop owners failed verification"))?;
        let inputs = if is_for {
            slots
                .group(&groups[0], self.task, activation.invocation.id())
                .ok_or_else(|| malformed("caught loop has no operand group"))?
                .into_iter()
                .map(|owner| Ok(ledger.holding(owner, self.heap.pages())?.map(|_| owner)))
                .collect::<Result<Vec<_>, HeapFault>>()?
                .into_iter()
                .flatten()
                .collect()
        } else {
            Vec::new()
        };
        let released = ledger.release_cost(&inputs, self.heap.pages())?;
        drop(ledger);
        let values = if is_for {
            [(Measure::Release(groups.into()), i128::from(released))].into()
        } else {
            Default::default()
        };
        if !self.publish_source_event(
            frame.state.callee,
            activation,
            Prefix {
                interval,
                start,
                next,
                facts: LeafExecution(
                    [(
                        at,
                        LeafFacts {
                            values,
                            ..Default::default()
                        },
                    )]
                    .into(),
                ),
                allowance: Allowance {
                    occurrences: vec![],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: false,
                    depth: 0,
                    safe_point: self.charged,
                },
            },
            ExactRoots {
                inputs,
                ..Default::default()
            },
            Successor::Normal,
            (),
            |_, _, edits, ()| assert!(edits.is_empty()),
        )? {
            return Err(malformed("caught loop Commit publication refused"));
        }
        Ok(())
    }
}
