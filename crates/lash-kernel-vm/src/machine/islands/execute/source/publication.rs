//! The resource owner and canonical journal commit at the same source frontier.
use super::*;
use crate::admission::{Allowance, SourceResources};
use lash_kernel_state::kernel_internals::logical::{MemberEdit, Program};
use room::publication::{Position, PublicationError};
use std::sync::Arc;

pub(super) struct Prefix {
    pub(super) interval: Arc<room::CompiledInterval>,
    pub(super) facts: LeafExecution,
    pub(super) start: Position,
    pub(super) next: Position,
    pub(super) allowance: Allowance,
}
impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(super) fn publish_source(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        prefix: Prefix,
        make: impl Fn() -> Program,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>),
    ) -> Result<bool, HeapFault> {
        self.publish_source_prepared(
            callee,
            activation,
            prefix,
            make,
            |_| Ok(Some(())),
            |arena, edits, ()| semantic(arena, edits),
        )
    }

    pub(super) fn publish_source_prepared<S>(
        &mut self,
        callee: Callee<'_>,
        activation: &mut Activation,
        mut prefix: Prefix,
        make: impl Fn() -> Program,
        prepare: impl FnOnce(&mut Arena) -> Result<Option<S>, HeapFault>,
        semantic: impl FnOnce(&mut Arena, Vec<MemberEdit>, S),
    ) -> Result<bool, HeapFault> {
        let source = &self.exe.code(callee.plan.code).source_room;
        let store = self.logical.ledger();
        if let Some(frontier) = &mut activation.frontier
            && frontier.position() != &prefix.start
        {
            frontier.handoff(&mut store.borrow_mut(), prefix.start.clone())?;
        }
        let ledger = store.borrow();
        let document = self
            .exe
            .instances
            .get(&callee.link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        if activation.frontier.is_none() {
            activation.frontier = Some(
                source
                    .frontier(
                        &ledger,
                        self.heap,
                        &activation.invocation,
                        callee.link,
                        kernel,
                        prefix.start,
                    )
                    .map_err(|_| malformed("source frontier failed authentication"))?,
            );
        }
        let frontier = activation
            .frontier
            .as_mut()
            .ok_or_else(|| malformed("source frontier disappeared"))?;
        let safe_point = self.charged;
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: self.base + u64::from(self.depth),
            limit: self.call_depth,
            safe_point,
        };
        let Some(resource_facts) = resources.facts(&prefix.allowance) else {
            return Ok(false);
        };
        let Ok(evaluated) = frontier.evaluate_interval(
            prefix.interval.clone(),
            &mut prefix.facts,
            &ledger,
            self.arena,
            self.heap,
            make(),
            prefix.next.clone(),
            prefix.allowance.clone(),
            resource_facts,
        ) else {
            return Ok(false);
        };
        let Some(accepted) = evaluated.admit_interval(
            frontier,
            &ledger,
            self.arena,
            resources,
            &prefix.allowance,
            self.memory_bound,
        )?
        else {
            return Ok(false);
        };
        let Some(storage) = prepare(self.arena)? else {
            return Ok(false);
        };
        let prepared = match accepted.prepare_publication(
            frontier,
            &ledger,
            self.arena
                .registration()
                .map_err(|error| malformed(&error.to_string()))?,
            self.heap,
            make(),
            prefix.interval.exit(),
            &prefix.next,
        ) {
            Ok(prepared) => prepared,
            Err(PublicationError::Miss(_)) => return Ok(false),
            Err(PublicationError::Heap(error)) => return Err(error),
        };
        drop(ledger);
        let committed = prepared
            .publish(frontier, &mut store.borrow_mut(), |arena, edits| {
                semantic(arena, edits, storage)
            })
            .map_err(|error| match error {
                PublicationError::Heap(error) => error,
                PublicationError::Miss(reason) => malformed(reason),
            })?;
        debug_assert!(committed.materializations.entities().is_empty());
        self.logical_memory.set(committed.balance.total()?);
        Ok(true)
    }
}
