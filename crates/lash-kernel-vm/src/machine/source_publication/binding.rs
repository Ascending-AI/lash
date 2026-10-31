//! Precise local bindings consume their actual operand in the accepted journal.
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::compile::room::{ExitKind, Phase, Point};
use crate::machine::stream::Running;
use crate::machine::{Control, Halt, KernelMachine, SlotState, fault};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, PublicationValue};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_binding(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let Op::Bind { local, src } = running.stream.ops[pc as usize] else {
            return Ok(false);
        };
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, link) = {
            let frame = self.frame(running.task)?;
            // Loop headers and active departures need their own exact runtime
            // occurrence. This cohort has neither; it never invents one.
            if frame.cleaning.is_some()
                || frame.control.iter().any(|control| {
                    matches!(
                        control,
                        Control::For { .. } | Control::While { .. } | Control::Try { .. }
                    )
                })
            {
                return Ok(false);
            }
            if running
                .exe
                .code(frame.code)
                .slots_by_at
                .get(local.at as usize)
                != Some(&local.slot)
            {
                return Err(malformed("source binding has another frame slot").into());
            }
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
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
        let Some(bank) = self.regs.prepare_source_transfer(src)? else {
            return Ok(false);
        };
        let input = bank.owner();
        let value = bank.value();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.precise-binding");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
        local.slot.encode(&mut encoding);
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
            .ok_or_else(|| malformed("precise source has no entered invocation"))?;
        let output = source
            .binding_root(local.slot, running.task, roots.invocation().id())
            .ok_or_else(|| malformed("source binding has no canonical owner"))?;
        let Some(slot) = frame
            .slots
            .prepare_binding(local.at, SlotState::Value(value))?
        else {
            return Ok(false);
        };
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops: vec![],
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
                        at.clone(),
                        LeafFacts {
                            dependencies: value.handle().into_iter().collect(),
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
                    outputs: vec![(output, PublicationValue::Existing(input))],
                    ..Default::default()
                }])
            },
            Reserve(move |_: &mut Arena| Ok(Some((bank, slot)))),
            |_, _, edits, (bank, slot)| {
                debug_assert!(edits.is_empty());
                bank.publish();
                slot.publish();
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            #[cfg(test)]
            {
                self.storage.value_moves += 1;
            }
            crate::machine::islands::witness::precise_binding_published(value);
        }
        Ok(published)
    }
}
