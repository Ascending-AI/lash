//! A do statement releases its actual RHS before the operand bank clears.
use super::*;
use crate::compile::room::{
    ExitKind, Measure, Owner as Selector, Phase, Point, publication::Occurrence,
};
use crate::machine::{Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::JournalEvent;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_end(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        if !matches!(running.stream.ops[pc as usize], Op::End) {
            return Ok(false);
        }
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        if site.as_ref() != &running.stmt.site {
            return Err(malformed("source end names another statement").into());
        }
        let (source, link, loops, invocation) = {
            let frame = self.frame(running.task)?;
            let Some(roots) = &frame.logical else {
                return Ok(false);
            };
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(false);
            };
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                loops,
                roots.invocation().id(),
            )
        };
        let Ok(unpaid) = source.next_after_commit(site) else {
            return Ok(false);
        };
        let at = Point {
            site: (**site).clone(),
            phase: Phase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        self.logical_flush()?;
        let rhs = site.child(0);
        let Some(bank) = self.regs.prepare_source_end(&rhs)? else {
            return Ok(false);
        };
        let owners = source
            .owner_slots()
            .map_err(|_| malformed("source end owners failed verification"))?;
        let expected = owners
            .root(&Selector::Operand(rhs), running.task, invocation)
            .ok_or_else(|| malformed("source end has no canonical RHS owner"))?;
        let input = bank.owner();
        if input != expected {
            return Err(malformed("source end has another RHS owner").into());
        }
        let value = bank.value();
        let released = self
            .logical
            .ledger()
            .borrow()
            .release_cost(&[input], self.heap.pages())?;
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("source end event disappeared"))?
            .inputs
            .clone();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-end");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(running.task.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            + usize::from(running.task != doc::TaskId::MAIN || !self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("source end has no entered invocation"))?;
        if roots.invocation().id() != invocation {
            return Err(malformed("source end preparation names another invocation").into());
        }
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation,
                loops,
                action: None,
                departure: None,
            },
        };
        let next = Position {
            at: at.clone(),
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
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
                        at,
                        LeafFacts {
                            dependencies: value.handle().into_iter().collect(),
                            values: [(Measure::Release(release), i128::from(released))].into(),
                            ..Default::default()
                        },
                    )]
                    .into(),
                ),
                start,
                next,
                allowance: Allowance {
                    occurrences: vec![],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: false,
                    depth: 0,
                    safe_point,
                },
            },
            || {
                Program::new(vec![JournalEvent {
                    inputs: vec![input],
                    ..Default::default()
                }])
            },
            Reserve(move |_: &mut Arena| Ok(Some(bank))),
            |_, _, edits, bank| {
                debug_assert!(edits.is_empty());
                bank.publish();
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_end_published(value.handle().is_some());
        }
        Ok(published)
    }
}
