//! A nested Leave releases only that scope's genuine source owners.
use super::*;
use room::{ExitKind, Phase as SourcePhase, Point};
use std::collections::BTreeSet;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_nested_leave(
        &mut self,
        frame: &mut Values<'_, '_>,
        block: usize,
    ) -> Result<bool, HeapFault> {
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let closing = &self
            .exe
            .block(frame.state.callee.compiled.blocks[block].id)
            .site;
        if closing == &source.site {
            return Ok(false);
        }
        // A parent position with no next source event refuses, as the checked
        // path does.
        let (loop_body, dependency) = match frame.state.control.iter().rev().nth(1) {
            Some(Position::Block(parent, next)) => {
                let parent = self
                    .exe
                    .block(frame.state.callee.compiled.blocks[*parent].id);
                if let Some(stmt) = parent.stmts.get(*next)
                    && source.first_event(&self.exe.stmt(*stmt).site).is_err()
                {
                    return Ok(false);
                }
                (None, None)
            }
            Some(Position::Loop {
                id,
                started,
                cursor: None,
                ..
            }) => {
                let crate::compile::Form::While { site, body, .. } = &self.exe.stmt(*id).form
                else {
                    return Err(malformed("scope parent has another loop form"));
                };
                if *body != frame.state.callee.compiled.blocks[block].id || *started == 0 {
                    return Err(malformed("scope is not an entered while body"));
                }
                if source.first_event(&site.child(0)).is_err() {
                    return Ok(false);
                }
                (Some(ScopeLoop::While), None)
            }
            Some(Position::Loop {
                id,
                started,
                cursor: Some(cursor),
                guard: None,
                ..
            }) => {
                let crate::compile::Form::For {
                    site,
                    body,
                    via: None,
                    ..
                } = &self.exe.stmt(*id).form
                else {
                    return Ok(false);
                };
                if *body != frame.state.callee.compiled.blocks[block].id || *started == 0 {
                    return Err(malformed("scope is not an entered for body"));
                }
                let Some(Ok((_, next))) = cursor.resident_step(self.arena) else {
                    return Ok(false);
                };
                let continued = next.is_some();
                if source.next_after_for_cursor(site, continued).is_err() {
                    return Ok(false);
                }
                (
                    Some(ScopeLoop::For { continued }),
                    cursor.walked().and_then(|value| value.handle()),
                )
            }
            _ => return Ok(false),
        };
        debug_assert!(loop_body.is_none() || frame.state.control.len() >= 2);
        debug_assert!(!matches!(loop_body, Some(ScopeLoop::For { .. })) || dependency.is_some());
        let leave = Point {
            site: closing.clone(),
            phase: SourcePhase::Leave,
        };
        let Ok(interval) = source.compile_event(&leave, ExitKind::Normal) else {
            return Ok(false);
        };
        if !interval.scope_release(source, closing)
            || dependency.is_some_and(|handle| self.arena.content_version(handle).is_none())
        {
            return Ok(false);
        }
        self.source_bindings(frame)?;
        self.logical.flush(self.arena, self.heap)?;
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("nested scope has no source activation"))?;
        if activation.frontier.is_none() {
            return Ok(false);
        }
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("nested scope owners failed verification"))?;
        let store = self.logical.ledger();
        let mut ledger = store.borrow_mut();
        let live = ledger.frame_owners(self.task, activation.invocation.id(), self.heap.pages())?;
        let mut inputs = BTreeSet::new();
        for selector in [
            room::Owner::Scope(closing.clone()),
            room::Owner::Operands(closing.clone()),
        ] {
            let owners = slots
                .select_group(&selector, self.task, activation.invocation.id(), &live)
                .ok_or_else(|| malformed("nested scope has no canonical owner group"))?;
            for owner in owners {
                if ledger.holding(owner, self.heap.pages())?.is_some() {
                    inputs.insert(owner);
                }
            }
        }
        let inputs = inputs.into_iter().collect::<Vec<_>>();
        // The proven prefix's shared admission: a release adds no peak and no
        // charge, so only the current envelope and window. Its journal is the
        // groups' held owners, released in place.
        let depth = (self.base + u64::from(self.depth)).saturating_sub(1);
        if ledger.balance()?.total()? > self.memory_bound
            || self.charged > self.bound
            || depth > self.call_depth
            || !ledger.release(&inputs, self.heap.pages())?
        {
            return Ok(false);
        }
        let total = ledger.balance()?.total()?;
        drop(ledger);
        frame
            .state
            .source
            .as_mut()
            .and_then(|activation| activation.frontier.as_mut())
            .ok_or_else(|| malformed("nested scope frontier disappeared"))?
            .proven()?;
        self.logical_memory.set(total);
        for at in &frame.state.callee.compiled.blocks[block].declares {
            frame.slots[*at as usize] = SlotState::Empty;
        }
        #[cfg(any(test, feature = "testing"))]
        {
            crate::machine::islands::witness::scope_published();
            match loop_body {
                Some(ScopeLoop::While) => {
                    crate::machine::islands::witness::while_scope_published(false)
                }
                Some(ScopeLoop::For { continued }) => {
                    crate::machine::islands::witness::for_scope_published(false, continued)
                }
                None => {}
            }
        }
        Ok(true)
    }
}
