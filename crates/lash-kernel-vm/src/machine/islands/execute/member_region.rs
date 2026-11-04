//! A private source prefix binds once, spends shared room once and installs
//! final member bodies once. A miss leaves the original source entry intact.
use crate::admission::SourceResources;
use crate::compile::room::publication::{
    Frontier,
    member_region::{MemberCompletion, MemberRegion},
};
use crate::compile::{CodeId, Executable};
use crate::machine::SlotState;
use crate::observe::ChargeTape;
use lash_kernel_doc::heap::HeapFault;
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::{
        PagedHeap,
        logical::{Invocation, Ledger},
    },
};

#[cfg(test)]
thread_local! { static COMPOSED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; static PREFIXES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) }; }
#[cfg(test)]
thread_local! { static STATEMENTS: std::cell::RefCell<Option<Vec<usize>>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
pub(crate) fn begin_statement_publications() {
    STATEMENTS.with(|counts| *counts.borrow_mut() = Some(Vec::new()));
}
#[cfg(test)]
pub(crate) fn take_statement_publications() -> Vec<usize> {
    STATEMENTS.with(|counts| counts.borrow_mut().take().unwrap_or_default())
}

#[cfg(test)]
pub(crate) fn take_publications() -> (u64, u64) {
    (
        COMPOSED.with(|n| n.replace(0)),
        PREFIXES.with(|n| n.replace(0)),
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "one actual source entry, storage owner and shared resource window"
)]
pub(crate) fn execute(
    plan: &MemberRegion,
    exe: &Executable,
    (code, slots): (CodeId, &[SlotState]),
    invocation: &Invocation,
    frontier: &mut Frontier,
    ledger: &mut Ledger,
    arena: &mut Arena,
    heap: &PagedHeap,
    resources: SourceResources<'_>,
    memory_bound: u64,
    tape: Option<&ChargeTape>,
) -> Result<Option<MemberCompletion>, HeapFault> {
    if !plan.starts_at(frontier) {
        return Ok(None);
    }
    let Some(activation) = plan
        .placement()
        .capture(code, invocation, ledger, heap, slots)?
    else {
        return Ok(None);
    };
    for count in (1..=plan.len()).rev() {
        let Some(bound) =
            plan.bind_prefix(exe, &activation, invocation, ledger, arena, heap, count)?
        else {
            continue;
        };
        let registration = arena
            .registration()
            .map_err(|error| HeapFault::Malformed(error.to_string()))?;
        let Some(prepared) = bound.prepare(
            frontier,
            ledger,
            registration,
            heap,
            &resources,
            memory_bound,
        )?
        else {
            continue;
        };
        let Some(accepted) = prepared.admit(resources)? else {
            return Ok(None);
        };
        let completion = accepted.publish_members(frontier, ledger, tape)?;
        #[cfg(test)]
        {
            PREFIXES.with(|n| n.set(n.get() + 1));
            STATEMENTS.with(|counts| {
                if let Some(counts) = &mut *counts.borrow_mut() {
                    counts.push(completion.statements);
                }
            });
            if completion.statements > 1 {
                COMPOSED.with(|n| n.set(n.get() + 1));
            }
        }
        return Ok(Some(completion));
    }
    Ok(None)
}

use super::*;
use std::sync::Arc;
/// Physical entry/jump placement is separate from the source certificate.
pub(crate) struct MemberEntry {
    region: Arc<MemberRegion>,
    block: usize,
    index: usize,
    first_packet: usize,
    after: Box<[usize]>,
}
impl MemberEntry {
    pub(in crate::machine::islands) fn compile(
        exe: &Executable,
        placement: Arc<crate::compile::room::placement::BindingPlacement>,
        block: usize,
        physical: &super::super::compiled::Block,
        index: usize,
    ) -> Option<Self> {
        let statements = exe.block(physical.id).stmts.get(index..)?;
        let region = Arc::new(MemberRegion::compile_tail(exe, placement, statements)?);
        let after = (1..=region.len())
            .map(|count| {
                physical
                    .starts
                    .get(index + count)
                    .copied()
                    .unwrap_or(physical.end)
            })
            .collect();
        Some(Self {
            region,
            block,
            index,
            first_packet: *physical.starts.get(index)?,
            after,
        })
    }
    pub(in crate::machine::islands) fn packet(&self) -> usize {
        self.first_packet
    }
}
impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    /// Call before packet fetch/Begin. A decline keeps that same source entry;
    /// a publication returns the physical jump for its authenticated exit.
    pub(super) fn private_members(
        &mut self,
        frame: &mut Values<'_, '_>,
        entry: &MemberEntry,
    ) -> Result<Option<usize>, Stop> {
        if OBSERVING
            || self.spent()
            || self.deferred_end.is_some()
            || self.arena.boundary_due()
            || frame.state.recovery
            || frame.state.may_clean
            || frame.state.statement.is_some()
            || frame.state.boundary.is_some()
            || frame.state.pc != entry.first_packet
            || !matches!(frame.state.control.last(), Some(Position::Block(block, index)) if *block == entry.block && *index == entry.index)
        {
            return Ok(None);
        }
        let Some(activation) = frame.state.source.as_ref() else {
            return Ok(None);
        };
        if activation.original.is_some() {
            return Ok(None);
        }
        let invocation = activation.invocation.clone();
        let callee = frame.state.callee;
        if callee.plan.code != entry.region.placement().code() {
            return Ok(None);
        }
        let transfer = self
            .logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[]);
        transfer.map_err(|error| self.halt(error.into()))?;
        let store = self.logical.clone();
        let document = self
            .exe
            .instances
            .get(&callee.link)
            .ok_or_else(|| self.halt(fault("member source document is absent")))?;
        let kernel = lash_kernel_doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| self.halt(fault("member source kernel is unsupported")))?;
        let occurrence = crate::compile::room::publication::Occurrence {
            task: self.task,
            invocation: invocation.id(),
            loops: self.source_loops(&frame.state.control),
            action: None,
        };
        let mut frontier = entry
            .region
            .source()
            .frontier(
                &store.borrow(),
                self.heap,
                &invocation,
                callee.link,
                kernel,
                entry.region.start(occurrence),
            )
            .map_err(|_| self.halt(fault("private member source entry is invalid")))?;
        let safe_point = self.charged;
        let resources = SourceResources {
            charged: &mut self.charged,
            counters: self.counters,
            bound: self.bound,
            slice: self.slice_end,
            depth: (self.base + u64::from(self.depth)).saturating_sub(1),
            limit: self.call_depth,
            safe_point,
        };
        let result = execute(
            &entry.region,
            self.exe,
            (callee.plan.code, frame.slots),
            &invocation,
            &mut frontier,
            &mut store.borrow_mut(),
            self.arena,
            self.heap,
            resources,
            self.memory_bound,
            self.recorder,
        )
        .map_err(|error| self.halt(error.into()))?;
        let Some(completion) = result else {
            return Ok(None);
        };
        let Some(Position::Block(_, next)) = frame.state.control.last_mut() else {
            unreachable!("private member entry retains its block");
        };
        *next = entry.index + completion.statements;
        frame
            .state
            .source
            .as_mut()
            .unwrap_or_else(|| unreachable!("private member source activation"))
            .frontier = Some(frontier);
        self.logical_memory.set(
            completion
                .balance
                .total()
                .map_err(|error| self.halt(error.into()))?,
        );
        self.end_statement(frame);
        self.covered = false;
        #[cfg(any(test, feature = "perf-witness"))]
        execution::statements(
            &self.scratch.census,
            Owner::RegisterProgram,
            completion.statements as u64,
            self.scratch.counting,
        );
        Ok(Some(entry.after[completion.statements - 1]))
    }
}
