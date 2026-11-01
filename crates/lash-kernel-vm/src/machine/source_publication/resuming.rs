//! A caller adopts its existing result and continuation in one source event.
use super::*;
use crate::compile::{
    Executable, NestedKind, StmtId,
    room::{ExitKind, Owner as Selector, Phase, Point},
};
use crate::machine::{
    Halt, KernelMachine, fault,
    logical_held::Held,
    registers::{ResumeAt, ResumeValue},
    stream::Running,
};
use lash_kernel_state::{
    kernel_internals::logical::{JournalEvent, Owner, PublicationValue},
    paged::PhaseRecord,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_call_resume(
        &mut self,
        task_id: doc::TaskId,
        exe: &Executable,
        value: doc::Value,
    ) -> Result<Option<(StmtId, u32)>, Halt> {
        self.logical_flush_controls(task_id, exe)?;
        self.logical_flush()?;
        let (source, link, invocation, roots, id, pc, at, occurrence, saved) = {
            let frame = self.frame(task_id)?;
            let (Some(id), Some(act), Some(pending), Some(logical), Some(frontier)) = (
                frame.awaiting,
                frame.nested,
                frame.pending.peek(),
                frame.logical.as_ref(),
                frame.source_frontier.as_ref(),
            ) else {
                return Ok(None);
            };
            let running = Running::new(task_id, exe, id);
            let entry = running
                .stream
                .nested
                .get(act.ix())
                .ok_or_else(|| fault("call resumption has no nested entry"))?;
            if !entry.guest
                || !matches!(entry.kind, NestedKind::Call(_))
                || pending.phase != PhaseRecord::Called
                || pending.site != entry.suffix
                || pending.roles.len() != entry.roles.len()
                || !exe.code(frame.code).charged
            {
                return Ok(None);
            }
            let Some(loops) = source_loops(frame, exe)? else {
                return Ok(None);
            };
            let position = frontier.position();
            if position.unpaid.is_none()
                || position.occurrence.invocation != logical.invocation().id()
                || position.occurrence.task != task_id
                || position.occurrence.loops != loops
                || !position
                    .occurrence
                    .action
                    .as_ref()
                    .is_some_and(|(point, _)| {
                        point.site == entry.site && point.phase == Phase::Enter
                    })
            {
                return Ok(None);
            }
            let mut saved = Vec::new();
            saved
                .try_reserve(entry.roles.len() + 1)
                .map_err(|_| HeapFault::Exhausted {
                    resource: doc::heap::HostResource::StagingBytes,
                })?;
            for role in &entry.roles {
                let held = pending
                    .roles
                    .iter()
                    .find(|saved| saved.site == role.suffix)
                    .ok_or_else(|| malformed("resuming call lost its admitted source role"))?;
                saved.push((role.reg, Arc::new(role.site.clone()), held.value));
            }
            saved.push((entry.dst, Arc::new(entry.site.clone()), value));
            let pc = running
                .stmt
                .ops
                .start
                .checked_add(entry.resume)
                .ok_or_else(|| malformed("call resume coordinate overflow"))?;
            let Some(expected) = running.stmt.source_roles_at(running.stream, pc) else {
                return Ok(None);
            };
            if expected.len() != saved.len()
                || expected.iter().any(|(reg, site)| {
                    !saved
                        .iter()
                        .any(|(actual, at, _)| actual == reg && at == site)
                })
            {
                return Err(
                    malformed("resuming call bank differs from its source projection").into(),
                );
            }
            (
                Arc::clone(&exe.code(frame.code).source_room),
                frame.link,
                logical.invocation().clone(),
                logical.bank(),
                id,
                pc,
                Point {
                    site: entry.site.clone(),
                    phase: Phase::Restore,
                },
                position.occurrence.clone(),
                saved,
            )
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(None);
        };
        let Ok(unpaid) = source.next_after_commit(&at.site) else {
            return Ok(None);
        };
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("call resumption source owners failed verification"))?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut values = Vec::new();
        let mut dependencies = Vec::new();
        inputs
            .try_reserve(saved.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        outputs
            .try_reserve(saved.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        values
            .try_reserve(saved.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        dependencies
            .try_reserve(saved.len())
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        for (reg, site, value) in saved {
            let input = if *site == at.site {
                Owner::Incoming(task_id)
            } else {
                slots
                    .continuation(&site, task_id, invocation.id())
                    .ok_or_else(|| malformed("resumption has no continuation owner"))?
            };
            let output = slots
                .root(
                    &Selector::Operand((*site).clone()),
                    task_id,
                    invocation.id(),
                )
                .ok_or_else(|| malformed("resumption has no operand owner"))?;
            let Some(holding) = ledger.holding(input, self.heap.pages())? else {
                return Ok(None);
            };
            inputs.push(input);
            outputs.push((output, PublicationValue::Existing(input)));
            dependencies.extend(value.handle());
            values.push(ResumeValue {
                reg,
                site,
                held: Held {
                    value,
                    holding,
                    owner: Some(output),
                },
            });
        }
        drop(ledger);
        let running = Running::new(task_id, exe, id);
        let Some(bank) = self.regs.prepare_source_resume(
            ResumeAt {
                task: task_id,
                invocation: invocation.id(),
                statement: running.stmt.site.clone(),
                pc,
            },
            roots,
            running.stmt.regs,
            values,
        )?
        else {
            return Ok(None);
        };
        let document = exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("resuming source has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("resuming source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.call-resume");
        at.site.encode(&mut encoding);
        invocation.frame().encode(&mut encoding);
        pc.encode(&mut encoding);
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence,
        };
        let next = Position {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let program = Program::new(vec![JournalEvent {
            inputs,
            outputs,
            ..Default::default()
        }]);
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(task_id.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            + usize::from(task_id != doc::TaskId::MAIN || !self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        if frame.invocation != invocation.id() {
            return Err(malformed("resuming preparation names another invocation").into());
        }
        let awaiting = &mut frame.awaiting;
        let nested = &mut frame.nested;
        let pending = &mut frame.pending;
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
            &invocation,
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at,
                        LeafFacts {
                            dependencies,
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
            || program.clone(),
            Reserve(move |_: &mut Arena| Ok(Some(bank))),
            |_, _, edits, bank| {
                debug_assert!(edits.is_empty());
                bank.publish();
                *awaiting = None;
                *nested = None;
                pending.take();
            },
        )?;
        if published {
            self.regs.bind_source_resume(&store.borrow());
            #[cfg(any(test, feature = "testing"))]
            crate::machine::islands::witness::call_resume_published();
        }
        Ok(published.then_some((id, pc)))
    }
}
