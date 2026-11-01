//! A guest action observes its pinned arguments before the callee transfers them.
use super::*;
use crate::compile::{
    Nested,
    room::{ExitKind, Phase, Point, publication::Occurrence},
};
use crate::machine::{Halt, KernelMachine, fault, stream::Running};
use lash_kernel_state::kernel_internals::logical::JournalEvent;
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
        self.logical_flush()?;
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
                departure: None,
            },
        };
        let next = Position {
            unpaid: Some(Point {
                site: entry.site.clone(),
                phase: Phase::Commit,
            }),
            ..start.clone()
        };
        let program = Program::new(vec![JournalEvent {
            borrowed,
            ..Default::default()
        }]);
        let safe_point = self.charged;
        let source_action = &mut frame.source_action;
        let published = Context {
            admission: Admission::Exact,
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
            memory_bound: self.bounds.memory,
            logical_memory: &self.logical_memory,
        }
        .publish(
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
            || program.clone(),
            Reserve(move |_: &mut Arena| Ok(Some(bank))),
            |_, _, edits, _bank| {
                debug_assert!(edits.is_empty());
                *source_action = Some((entry.occurrence, occurrence));
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::call_enter_published();
        }
        Ok(published.then_some(occurrence))
    }
}
