//! Member reads publish their checked operands and canonical result together.
use super::*;
use crate::compile::room::publication::{ExactRoots, Occurrence, Successor};
use crate::compile::room::{ExitKind, Measure, Owner as Selector, Phase, Point};
use crate::machine::stream::Running;
use crate::machine::{Halt, KernelMachine, fault};
use lash_kernel_exec::Op;
use lash_kernel_state::kernel_internals::logical::{Holding, PublicationValue, RootValue};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_read(
        &mut self,
        running: &Running<'_>,
        pc: u32,
        value: doc::Value,
    ) -> Result<bool, Halt> {
        let op = running.stream.ops[pc as usize];
        if !matches!(op, Op::Field { .. } | Op::Index { .. }) {
            return Ok(false);
        }
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let (source, link, loops) = {
            let frame = self.frame(running.task)?;
            let Some(loops) = source_loops(frame, running.exe)? else {
                return Ok(false);
            };
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                loops,
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
        let Some(bank) = self.regs.prepare_source_output(running.stream, pc)? else {
            return Ok(false);
        };
        let output = bank.owner();
        let inputs = bank.owners();
        let values = bank.values();
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("read event disappeared"))?
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
        encoding.text("lash.source.precise-read");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        let mut arena = self.arena.borrow_mut();
        let holding = Holding::from_native(&mut arena, self.heap.pages(), &value)?;
        let released = self.logical.ledger().borrow().release_cost_preserving(
            &mut arena,
            &self.heap,
            &inputs,
            &[(output, RootValue::Value(&value))],
        )?;
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
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: running.task,
                invocation: roots.invocation().id(),
                loops,
                action: None,
            },
        };
        let next = Position {
            at: at.clone(),
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
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
        .publish_event(
            roots.invocation(),
            &mut frame.source_frontier,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at.clone(),
                        LeafFacts {
                            dependencies: values
                                .iter()
                                .chain(std::iter::once(&value))
                                .filter_map(doc::Value::handle)
                                .collect(),
                            values: [
                                (
                                    Measure::Local(Selector::Operand(at.site.clone())),
                                    i128::from(holding.bytes()),
                                ),
                                (Measure::Release(release), i128::from(released)),
                            ]
                            .into(),
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
            ExactRoots {
                inputs,
                outputs: vec![(output, PublicationValue::Native(value))],
                ..Default::default()
            },
            Successor::Normal,
            (bank, holding),
            |_, _, edits, (bank, holding)| {
                debug_assert!(edits.is_empty());
                bank.publish(value, holding);
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_read_published(matches!(
                op,
                Op::Index { .. }
            ));
        }
        Ok(published)
    }
}
