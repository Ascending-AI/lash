//! Exact evaluated events publish actual roots through the common boundary.
use super::*;
use crate::compile::room::publication::{ExactEvent, ExactRoots, Successor};

#[cfg(test)]
thread_local! { static PUBLISHED: Cell<[u64; 6]> = const { Cell::new([0; 6]) }; }
#[cfg(test)]
impl crate::machine::KernelMachine {
    pub(crate) fn take_exact_events(&self) -> [u64; 6] {
        PUBLISHED.with(|counts| counts.replace([0; 6]))
    }
}

impl Context<'_> {
    pub(in crate::machine) fn publish_external<S: BoundaryPreparation>(
        self,
        invocation: &Invocation,
        frontier: &mut Option<Frontier>,
        prefix: Prefix,
        prepare: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, Vec<MemberEdit>, S::Prepared),
    ) -> Result<bool, Halt> {
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        if let Some(frontier) = frontier.as_mut()
            && frontier.departs(&prefix.start)
        {
            frontier.handoff(&mut store.borrow_mut(), prefix.start.clone())?;
        }
        let ledger = store.borrow();
        if frontier.is_none() {
            *frontier = Some(
                self.source
                    .frontier(
                        &ledger,
                        self.heap,
                        invocation,
                        self.link,
                        self.kernel,
                        prefix.start.clone(),
                    )
                    .map_err(|_| malformed("external source frontier failed authentication"))?,
            );
        }
        let frontier = frontier
            .as_mut()
            .ok_or_else(|| malformed("external frontier disappeared"))?;
        let Ok(event) =
            ExactEvent::compile(self.source, Arc::clone(&prefix.interval), Successor::Normal)
        else {
            return Ok(false);
        };
        if prefix.allowance.safe_point != *self.resources.charged {
            return Ok(false);
        }
        let Some(resources) = self.resources.facts(&prefix.allowance) else {
            return Ok(false);
        };
        let Some((mut prepared, storage)) = prepare.prepare(
            self.arena,
            ExternalEntry {
                event: &event,
                frontier,
                ledger: &ledger,
                heap: self.heap,
                prefix,
                resources,
            },
        )?
        else {
            return Ok(false);
        };
        let memory_bound = match self.admission {
            Admission::Envelope(bound) => bound,
            Admission::Exact(mut memory) => {
                if memory.exceeded(prepared.peak()) {
                    prepared = prepared.collect(&ledger, self.heap)?;
                    if memory.exceeded(prepared.peak()) {
                        return Ok(false);
                    }
                }
                memory.admit(prepared.peak())?
            }
        };
        let Some(accepted) = prepared.admit(frontier, &ledger, self.resources, memory_bound)?
        else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted
            .publish_event(frontier, &mut store.borrow_mut(), |arena, edits| {
                semantic(arena, self.session, edits, storage)
            })
            .map_err(|error| match error {
                PublicationError::Heap(error) => error,
                PublicationError::Miss(reason) => malformed(reason),
            })?;
        self.logical_memory.set(balance.total()?);
        #[cfg(any(test, feature = "testing"))]
        crate::machine::islands::witness::source_loop_published(
            &frontier.position().occurrence.loops,
        );
        Ok(true)
    }

    pub(in crate::machine) fn publish_external_remaining<S: BoundaryPreparation>(
        self,
        invocation: &Invocation,
        frontier: &mut Option<Frontier>,
        prefix: Prefix,
        prepare: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, Vec<MemberEdit>, S::Prepared),
        remaining: (
            &mut Option<room::charges::AcceptedOriginalPrefix>,
            Option<&crate::observe::ChargeTape>,
        ),
    ) -> Result<bool, Halt> {
        let (receipt, tape) = remaining;
        let Some(original) = receipt.as_ref() else {
            return self.publish_external(invocation, frontier, prefix, prepare, semantic);
        };
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let Some(frontier) = frontier.as_mut() else {
            return Ok(false);
        };
        if prefix.allowance.safe_point != *self.resources.charged
            || !original.enter_event(
                frontier,
                &mut store.borrow_mut(),
                prefix.start.clone(),
                *self.resources.charged,
            )?
        {
            return Ok(false);
        }
        let ledger = store.borrow();
        let Ok(event) =
            ExactEvent::compile(self.source, Arc::clone(&prefix.interval), Successor::Normal)
        else {
            return Ok(false);
        };
        let Some(resources) = self.resources.facts(&prefix.allowance) else {
            return Ok(false);
        };
        let Some((mut prepared, storage)) = prepare.prepare_remaining(
            self.arena,
            ExternalEntry {
                event: &event,
                frontier,
                ledger: &ledger,
                heap: self.heap,
                prefix,
                resources,
            },
            original,
        )?
        else {
            return Ok(false);
        };
        let bound = match self.admission {
            Admission::Envelope(bound) => bound,
            Admission::Exact(mut memory) => {
                if memory.exceeded(prepared.peak()) {
                    prepared = prepared.collect(&ledger, self.heap)?;
                }
                memory.admit(prepared.peak())?
            }
        };
        let Some(accepted) = prepared.admit(frontier, &ledger, self.resources, bound)? else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted
            .publish_original_event(
                frontier,
                &mut store.borrow_mut(),
                receipt,
                |arena, edits| semantic(arena, self.session, edits, storage),
                tape,
            )
            .map_err(|error| match error {
                PublicationError::Heap(error) => error,
                PublicationError::Miss(reason) => malformed(reason),
            })?;
        self.logical_memory.set(balance.total()?);
        Ok(true)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "one exact evaluated source event"
    )]
    pub(in crate::machine) fn publish_event<S>(
        self,
        invocation: &Invocation,
        frontier: &mut Option<Frontier>,
        mut prefix: Prefix,
        roots: ExactRoots,
        successor: Successor,
        storage: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, Vec<MemberEdit>, S),
    ) -> Result<bool, Halt> {
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        if let Some(frontier) = frontier.as_mut()
            && frontier.departs(&prefix.start)
        {
            frontier.handoff(&mut store.borrow_mut(), prefix.start.clone())?;
        }
        let ledger = store.borrow();
        if frontier.is_none() {
            *frontier = Some(
                self.source
                    .frontier(
                        &ledger,
                        self.heap,
                        invocation,
                        self.link,
                        self.kernel,
                        prefix.start.clone(),
                    )
                    .map_err(|_| malformed("exact source frontier failed authentication"))?,
            );
        }
        let frontier = frontier
            .as_mut()
            .ok_or_else(|| malformed("exact frontier disappeared"))?;
        #[cfg(test)]
        let family = match prefix.start.at.phase {
            room::Phase::Restore => 5,
            room::Phase::Leave => 4,
            room::Phase::Test if roots.outputs.is_empty() => 3,
            _ if roots.outputs.iter().any(|(owner, _)| {
                matches!(
                    owner,
                    lash_kernel_state::kernel_internals::logical::Owner::Value {
                        slot: lash_kernel_state::kernel_internals::logical::Slot::Binding(_),
                        ..
                    }
                )
            }) =>
            {
                2
            }
            _ if roots.inputs.is_empty() => 0,
            _ => 1,
        };
        let Ok(event) = ExactEvent::compile(self.source, prefix.interval, successor) else {
            return Ok(false);
        };
        if prefix.allowance.safe_point != *self.resources.charged {
            return Ok(false);
        }
        let Some(facts) = self.resources.facts(&prefix.allowance) else {
            return Ok(false);
        };
        let Ok(bound) = event.bind(
            frontier,
            &ledger,
            self.arena,
            self.heap,
            &mut prefix.facts,
            roots,
            prefix.next,
            prefix.allowance,
            facts,
        ) else {
            return Ok(false);
        };
        let registration = self.arena.registration().map_err(native_fault)?;
        let mut prepared = match bound.prepare(frontier, &ledger, registration, self.heap) {
            Ok(prepared) => prepared,
            Err(PublicationError::Miss(_)) => return Ok(false),
            Err(PublicationError::Heap(error)) => return Err(error.into()),
        };
        let memory_bound = match self.admission {
            Admission::Envelope(bound) => bound,
            Admission::Exact(mut memory) => {
                if memory.exceeded(prepared.peak()) {
                    prepared = prepared.collect(&ledger, self.heap)?;
                    // A retirement cannot turn an already-held inbound value
                    // into an allocation refusal. Its exact handler releases
                    // that value before the statement's capacity verdict.
                    if prepared.releases_only() && memory.exceeded(prepared.peak()) {
                        return Ok(false);
                    }
                }
                memory.admit(prepared.peak())?
            }
        };
        let Some(accepted) = prepared.admit(frontier, &ledger, self.resources, memory_bound)?
        else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted
            .publish_event(frontier, &mut store.borrow_mut(), |arena, edits| {
                semantic(arena, self.session, edits, storage)
            })
            .map_err(|error| match error {
                PublicationError::Heap(error) => error,
                PublicationError::Miss(reason) => malformed(reason),
            })?;
        self.logical_memory.set(balance.total()?);
        #[cfg(test)]
        PUBLISHED.with(|counts| {
            let mut next = counts.get();
            next[family] += 1;
            counts.set(next);
        });
        #[cfg(any(test, feature = "testing"))]
        crate::machine::islands::witness::source_loop_published(
            &frontier.position().occurrence.loops,
        );
        Ok(true)
    }
}
