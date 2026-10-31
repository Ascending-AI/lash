//! Compiler joins publish their authenticated child receipt at the source Commit.
use super::*;
use crate::compile::{
    SourceJoin,
    room::{Measure, Phase, Point, publication::Occurrence},
};
use crate::machine::{Halt, KernelMachine, SlotState, fault, stream::Running};
use lash_kernel_exec::{LocalRef, Op};
use lash_kernel_state::kernel_internals::logical::{JournalEvent, PublicationValue};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_joins(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<(), Halt> {
        let scope = match running.stream.ops[pc as usize] {
            Op::LetEnd { local } => Some(local),
            _ => None,
        };
        let mut closed = scope.is_none();
        if let Some(joins) = running.stream.source_joins.get(&pc) {
            for join in joins {
                let scope = scope.filter(|_| {
                    running.stream.source_sites[pc as usize].as_ref() == Some(&join.to)
                });
                closed |= scope.is_some();
                if !self.publish_source_join(running, join, scope)? {
                    if let Some(local) = scope {
                        self.frame(running.task)?
                            .set_slot(local.at, SlotState::Empty);
                    }
                    self.regs.source_complete_join(join)?;
                }
            }
        }
        if !closed {
            return Err(malformed("expression scope close has no source join").into());
        }
        self.logical_flush()?;
        Ok(())
    }

    fn publish_source_join(
        &mut self,
        running: &Running<'_>,
        join: &SourceJoin,
        scope: Option<LocalRef>,
    ) -> Result<bool, Halt> {
        let (source, link, loops) = {
            let frame = self.frame(running.task)?;
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(false);
            };
            if let Some(local) = scope
                && running
                    .exe
                    .code(frame.code)
                    .slots_by_at
                    .get(local.at as usize)
                    != Some(&local.slot)
            {
                return Err(malformed("expression scope has another frame slot").into());
            }
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                loops,
            )
        };
        let Ok(unpaid) = source.next_after_commit(&join.to) else {
            return Ok(false);
        };
        let Ok(interval) = source.compile_join(&join.from, &join.to) else {
            return Ok(false);
        };
        let at = Point {
            site: (*join.to).clone(),
            phase: Phase::Commit,
        };
        self.logical_flush()?;
        let Some(bank) = self.regs.prepare_source_join(join)? else {
            return Ok(false);
        };
        let input = bank.input();
        let output = bank.output();
        let value = bank.value();
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
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
        let mut inputs = vec![input];
        let mut measures = std::collections::BTreeMap::new();
        let slot = if let Some(local) = scope {
            let binding = source
                .binding_root(local.slot, running.task, roots.invocation().id())
                .ok_or_else(|| malformed("expression scope has no canonical binding"))?;
            let store = self.logical.ledger();
            let ledger = store.borrow();
            let released = ledger.release_cost(&[binding], self.heap.pages())?;
            inputs.push(binding);
            for measure in interval
                .events()
                .next()
                .ok_or_else(|| malformed("join event disappeared"))?
                .delta
                .terms
                .keys()
            {
                if matches!(measure, Measure::Release(_)) {
                    measures.insert(measure.clone(), i128::from(released));
                }
            }
            let Some(slot) = frame.slots.prepare_release(local.at)? else {
                return Ok(false);
            };
            Some(slot)
        } else {
            None
        };
        let mut encoding = TokenWriter::new();
        encoding.seq(5);
        encoding.text("lash.source.precise-join");
        join.from.encode(&mut encoding);
        join.to.encode(&mut encoding);
        input.encode(&mut encoding);
        output.encode(&mut encoding);
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
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
                            values: measures,
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
                    inputs: inputs.clone(),
                    outputs: vec![(output, PublicationValue::Existing(input))],
                    ..Default::default()
                }])
            },
            Reserve(|_: &mut Arena| Ok(Some((bank, slot)))),
            |_, _, edits, (bank, slot)| {
                debug_assert!(edits.is_empty());
                bank.publish();
                if let Some(slot) = slot {
                    slot.publish(SlotState::Empty);
                }
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_join_published(scope.is_some());
        }
        Ok(published)
    }
}
