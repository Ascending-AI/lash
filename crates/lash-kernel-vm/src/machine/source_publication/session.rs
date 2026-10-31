//! Precise session writes install their prepared binding under source acceptance.
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::compile::room::{ExitKind, Measure, Owner as Selector, Phase, Point};
use crate::machine::{Control, Halt, KernelMachine, fault, stream::Running};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, Owner, PublicationValue};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_precise_session_write(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<bool, Halt> {
        let op = running.stream.ops[pc as usize];
        let (Op::BindSession { name, src } | Op::WriteSession { name, src }) = op else {
            return Ok(false);
        };
        let bind = matches!(op, Op::BindSession { .. });
        let binding = &running.stream.names[name.ix()];
        if !bind
            && self.session.resident(&binding.key).is_none()
            && self
                .heap
                .load_session_bound_at(&binding.key)
                .map_err(crate::machine::write_halt)?
                .is_none()
        {
            return Ok(false);
        }
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, link) = {
            let frame = self.frame(running.task)?;
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
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
            )
        };
        if source.site.unit != lash_kernel_value::sites::Unit::Main
            || bind && source.persistent.get(site) != Some(&binding.name)
        {
            return Ok(false);
        }
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
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("session source owners failed verification"))?;
        let selector = if bind {
            Selector::SessionCell {
                at: at.site.clone(),
                name: binding.name.clone(),
            }
        } else {
            Selector::Visible {
                at: at.site.clone(),
                name: binding.name.clone(),
            }
        };
        let Some(output) = slots.root(&selector, running.task, roots.invocation().id()) else {
            return Ok(false);
        };
        if output != Owner::session(&binding.name) {
            return Ok(false);
        }
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let held = ledger.holding(output, self.heap.pages())?.is_some();
        if !bind && !held {
            return Ok(false);
        }
        let mut inputs = vec![input];
        let released = if held {
            inputs.push(output);
            ledger.release_cost(&[output], self.heap.pages())?
        } else {
            0
        };
        let mut measures = std::collections::BTreeMap::new();
        for measure in interval
            .events()
            .next()
            .ok_or_else(|| malformed("session event disappeared"))?
            .delta
            .terms
            .keys()
        {
            if matches!(measure, Measure::Release(_)) {
                measures.insert(measure.clone(), i128::from(released));
            }
        }
        drop(ledger);
        let storage = self.session.prepare_write(binding, value)?;
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.precise-session-write");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
        output.encode(&mut encoding);
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
            Reserve(|_: &mut Arena| Ok(Some((bank, storage)))),
            |_, session, edits, (bank, storage)| {
                debug_assert!(edits.is_empty());
                storage.publish(session);
                bank.publish();
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::session_published();
        }
        Ok(published)
    }
}
