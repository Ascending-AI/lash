//! A direct return transfers its real RHS, then leaves the same Invocation.
use super::*;
use room::{Phase as SourcePhase, Point};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_return_suffix(
        &mut self,
        frame: &mut Values<'_, '_>,
        value: Value,
        completion: &LibraryCompletion,
    ) -> Result<ValuePublication, HeapFault> {
        self.publish_departure_suffix(frame, value, completion, false)
    }

    pub(in crate::machine::islands::execute) fn publish_pending_return(
        &mut self,
        frame: &mut Values<'_, '_>,
        value: Value,
    ) -> Result<ValuePublication, HeapFault> {
        self.publish_departure_suffix(
            frame,
            value,
            &LibraryCompletion {
                units: 0,
                debt: None,
            },
            true,
        )
    }

    fn publish_departure_suffix(
        &mut self,
        frame: &mut Values<'_, '_>,
        value: Value,
        completion: &LibraryCompletion,
        suspended: bool,
    ) -> Result<ValuePublication, HeapFault> {
        if !(if suspended {
            self.source_departure_frame(frame)
        } else {
            self.source_return_frame(frame)
        }) || self.projection().is_some()
            || frame
                .state
                .source
                .as_ref()
                .and_then(|source| source.frontier.as_ref())
                .is_some_and(|frontier| frontier.position().unpaid.is_none())
        {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(returned) = frame.state.returned else {
            return Ok(ValuePublication::Unavailable);
        };
        if frame.state.statement != Some(returned.statement) {
            return Ok(ValuePublication::Unavailable);
        }
        let (stream, statement, _) = self.exe.statement(returned.statement);
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: statement.site.clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_departure_suffix(&at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let suspend = interval
            .events()
            .last()
            .filter(|event| event.point.phase == SourcePhase::Suspend)
            .map(|event| &event.point.site);
        if suspended != suspend.is_some() {
            return Ok(ValuePublication::Unavailable);
        }
        #[cfg(test)]
        if suspended && crate::machine::islands::witness::suspended_departure_declined() {
            return Ok(ValuePublication::Declined);
        }
        let target = |control: &Position| {
            matches!(control,
            Position::Try { cleanup, phase: Phase::Body | Phase::Catch }
                if suspend == Some(&self.exe.stmt(cleanup.id).site))
        };
        if suspended && !frame.state.control.iter().any(target) {
            return Err(malformed("packet departure has no entered cleanup"));
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
                .filter_map(|control| {
                    let Position::Block(block, _) = control else {
                        return None;
                    };
                    Some(
                        &self
                            .exe
                            .block(frame.state.callee.compiled.blocks[*block].id)
                            .site,
                    )
                }))
        {
            return Err(malformed(
                "return suffix differs from its entered source blocks",
            ));
        }
        let Some(roles) = statement.source_roles_at(stream, returned.pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some((_, rhs)) = roles.iter().find(|(reg, _)| reg.0 == returned.reg) else {
            return Ok(ValuePublication::Unavailable);
        };
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("return has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("return source owners failed verification"))?;
        let root = |selector| {
            slots
                .root(&selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("return has no canonical source owner"))
        };
        let input_selector = room::Owner::Operand((**rhs).clone());
        let input = root(input_selector)?;
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let store = self.logical.clone();
        let ledger = store.borrow();
        if ledger.holding(input, self.heap.pages())?.is_none() {
            return Ok(ValuePublication::Unavailable);
        }
        let loops = frame
            .state
            .control
            .iter()
            .filter_map(|control| match control {
                Position::Loop { id, started, .. } => {
                    Some((self.exe.stmt(*id).site.clone(), *started - 1))
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        #[cfg(test)]
        let loop_ordinal = loops.last().map(|(_, ordinal)| *ordinal);
        let Some(recipe) = (crate::machine::source_publication::return_storage::ReturnInput {
            source,
            invocation: &activation.invocation,
            task: self.task,
            at: &statement.site,
            carried: Some(
                crate::machine::source_publication::return_storage::Carried { rhs, input, value },
            ),
            safe_point: self.charged,
            loops,
            cursor: None,
        })
        .prepare(&ledger, self.arena, self.heap)?
        else {
            return Ok(ValuePublication::Declined);
        };
        drop(ledger);
        let mut recipe = recipe;
        recipe.allowance.units = completion.units;
        recipe.debt = completion.debt.clone();
        #[cfg(test)]
        if !frame.state.callee.plan.charged
            && self.base + u64::from(self.depth) == 2
            && crate::machine::islands::witness::take_body_return_decline()
        {
            return Ok(ValuePublication::Declined);
        }
        let published = self.publish_source_departure(frame.state.callee, activation, recipe)?;
        #[cfg(test)]
        if completion.units == 4 {
            eprintln!(
                "[DEBUG-r53-fee] return published={published} units={} charged={}",
                completion.units, self.charged
            );
        }
        #[cfg(any(test, feature = "testing"))]
        if published && !suspended {
            crate::machine::islands::witness::return_published();
            #[cfg(test)]
            if let Some(ordinal) = loop_ordinal {
                crate::machine::islands::witness::loop_returned(false, ordinal);
            }

            #[cfg(test)]
            if frame.state.control.len() > 1 {
                crate::machine::islands::witness::packet_scoped_returned();
            }
        }
        Ok(if published {
            ValuePublication::Published(value)
        } else {
            ValuePublication::Declined
        })
    }
}
