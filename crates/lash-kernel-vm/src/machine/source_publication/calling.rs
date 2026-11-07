//! A guest action observes its pinned arguments before the callee transfers them.
use super::*;
use crate::compile::room::publication::{ControlExit, ControlRoots};
use crate::compile::{
    Nested,
    room::{ExitKind, Phase, Point, publication::Occurrence},
};
use crate::machine::{Halt, KernelMachine, fault, stream::Running};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_call_enter(
        &mut self,
        running: &Running<'_>,
        entry: &Nested,
        arguments: lash_kernel_exec::RegRange,
    ) -> Result<Option<u64>, Halt> {
        let (source, link, loops) = {
            let frame = self.frame(running.task)?;
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(None);
            };
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                loops,
            )
        };
        let at = Point {
            site: entry.site.clone(),
            phase: Phase::Enter,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Action) else {
            return Ok(None);
        };
        self.logical_flush_controls(running.task, running.exe)?;
        self.publish_logical_meter()?;
        let Some(bank) = self.regs.prepare_source_arguments(arguments)? else {
            return Ok(None);
        };
        let borrowed = bank.owners();
        let values = bank.values();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("call entry has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("call entry kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.call-entry");
        at.site.encode(&mut encoding);
        encoding.seq(borrowed.len() as u64);
        for owner in &borrowed {
            owner.encode(&mut encoding);
        }
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let occurrence = task
            .counters
            .current(entry.occurrence)
            .ok_or_else(|| fault("unknown occurrence site"))?;
        let depth = (task.frames.len()
            + usize::from(running.task != doc::TaskId::MAIN || !self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("call entry has no entered invocation"))?;
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops,
                action: Some((at.clone(), occurrence)),
            },
        };
        let next = Position {
            unpaid: Some(Point {
                site: entry.site.clone(),
                phase: Phase::Commit,
            }),
            ..start.clone()
        };
        let safe_point = self.charged;
        let source_action = &mut frame.source_action;
        let published = Context {
            admission: Admission::Exact(MemoryLimit::of(
                self.bounds.memory_meter,
                &mut self.bounds.memory,
                &mut self.limiters,
            )),
            source: &source,
            link,
            kernel,
            logical: &self.logical,
            heap: &self.heap,
            arena: &mut arena,
            session: &mut self.session,
            resources: SourceResources {
                charged: &mut self.charged,
                counters: &mut task.counters,
                bound: if self.charging {
                    self.bounds.charge_limit
                } else {
                    u64::MAX
                },
                slice: self.slice_end,
                depth,
                limit: u64::from(self.bounds.call_depth),
                safe_point,
            },
            logical_memory: &self.logical_memory,
        }
        .publish_control(
            roots.invocation(),
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at.clone(),
                        LeafFacts {
                            dependencies: values.iter().filter_map(doc::Value::handle).collect(),
                            ..Default::default()
                        },
                    )]
                    .into(),
                ),
                start,
                next,
                allowance: Allowance {
                    occurrences: vec![entry.occurrence],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: false,
                    depth: 0,
                    safe_point,
                },
            },
            ControlRoots {
                borrowed,
                ..Default::default()
            },
            bank,
            |_, _, _bank| {
                *source_action = Some((entry.occurrence, occurrence));
            },
        )?;

        Ok(published.then_some(occurrence))
    }
}

impl Context<'_> {
    pub(in crate::machine) fn publish_control<S>(
        self,
        invocation: &Invocation,
        frontier: &mut Option<Frontier>,
        mut prefix: Prefix,
        roots: ControlRoots,
        storage: S,
        semantic: impl FnOnce(&mut Arena, &mut Session, S),
    ) -> Result<bool, Halt> {
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let store = self.logical.clone();
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
        let Ok(event) = ControlExit::compile(self.source, prefix.interval) else {
            return Ok(false);
        };
        if prefix.allowance.safe_point != *self.resources.charged {
            return Ok(false);
        }
        let Some(facts) = self.resources.facts(&prefix.allowance) else {
            return Ok(false);
        };
        let registration = self.arena.registration().map_err(native_fault)?;
        let Some(mut prepared) = event.prepare(
            self.source,
            frontier,
            invocation,
            &ledger,
            registration,
            self.heap,
            &mut prefix.facts,
            roots,
            prefix.next,
            prefix.allowance,
            facts,
        )?
        else {
            return Ok(false);
        };
        let memory_bound = match self.admission {
            Admission::Exact(mut memory) => {
                if memory.exceeded(prepared.peak()) {
                    prepared = prepared.collect(&ledger, self.heap)?;
                }
                memory.admit(prepared.peak())?
            }
        };
        let Some(accepted) = prepared.admit(frontier, &ledger, self.resources, memory_bound)?
        else {
            return Ok(false);
        };
        drop(ledger);
        let balance = accepted.publish_control(frontier, &mut store.borrow_mut(), |arena| {
            semantic(arena, self.session, storage)
        })?;
        self.logical_memory.set(balance.total()?);

        Ok(true)
    }
}
