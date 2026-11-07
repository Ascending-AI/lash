//! Conditional tests consume their actual condition before selecting control.
use super::*;
use crate::compile::room::publication::Occurrence;
use crate::compile::room::publication::{ExactRoots, Successor};
use crate::compile::room::{ExitKind, Measure, Owner as Selector, Phase, Point};
use crate::machine::stream::Running;
use crate::machine::{Control, Halt, KernelMachine, fault};
use lash_kernel_exec::Op;

use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};

impl KernelMachine {
    pub(in crate::machine) fn publish_source_test(
        &mut self,
        running: &Running<'_>,
        pc: u32,
    ) -> Result<Option<bool>, Halt> {
        let op = running.stream.ops[pc as usize];
        let (Op::Test { cond } | Op::Branch { cond, .. } | Op::JumpIfFalse { cond, .. }) = op
        else {
            return Ok(None);
        };
        let Some(Some(site)) = running.stream.source_sites.get(pc as usize) else {
            return Ok(None);
        };
        let (source, link, loops) = {
            let frame = self.frame(running.task)?;
            let Some(mut loops) = source_loops(frame, running.exe)? else {
                return Ok(None);
            };
            if matches!(op, Op::Test { .. }) {
                let Some(Control::While { stmt, started }) = frame.control.last() else {
                    return Err(malformed("source test has no active while").into());
                };
                let Some((loop_site, iteration)) = loops.last_mut() else {
                    return Err(malformed("source test has no loop occurrence").into());
                };
                if *stmt != running.id
                    || loop_site != site.as_ref()
                    || site.as_ref() != &running.stmt.site
                {
                    return Err(malformed("source test names another while").into());
                }
                // A header tests the next pass; body coordinates name the pass
                // already entered and use started - 1.
                *iteration = *started;
            }
            (
                Arc::clone(&running.exe.code(frame.code).source_room),
                frame.link,
                loops,
            )
        };
        let at = Point {
            site: (**site).clone(),
            phase: Phase::Test,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(None);
        };
        self.publish_logical_meter()?;
        let Some(bank) = self.regs.prepare_source_transfer(cond)? else {
            return Ok(None);
        };
        let doc::Value::Bool(taken) = bank.value() else {
            return Ok(None);
        };
        let successor = if matches!(op, Op::Test { .. }) {
            source.next_after_while_test(site, taken)
        } else {
            source.next_after_if_test(site, taken)
        };
        let Ok(unpaid) = successor else {
            return Ok(None);
        };
        let input = bank.owner();
        let released = self
            .logical
            .clone()
            .borrow()
            .release_cost(&[input], self.heap.pages())?;
        let document = running
            .exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-test");
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
            .ok_or_else(|| malformed("precise source has no entered invocation"))?;
        let expected = source
            .owner_slots()
            .map_err(|_| malformed("source test owners failed verification"))?
            .root(
                &Selector::Operand(site.child(0)),
                running.task,
                roots.invocation().id(),
            )
            .ok_or_else(|| malformed("source test has no canonical condition owner"))?;
        if input != expected {
            return Err(malformed("source test consumes another condition owner").into());
        }
        let block = if let Op::Branch { then_, else_, .. } = op {
            let block = crate::compile::BlockId(if taken { then_.0 } else { else_.0 });
            let selected = running.exe.block(block);
            if selected.site != site.child(if taken { 1 } else { 2 }) {
                return Err(malformed("source test selects another conditional block").into());
            }
            if selected.stmts.is_empty() {
                None
            } else {
                Some(frame.control.prepare_block(block)?)
            }
        } else {
            None
        };
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
                            values: [(
                                Measure::Release(vec![Selector::Operand(site.child(0))].into()),
                                i128::from(released),
                            )]
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
                inputs: vec![input],
                outputs: vec![],
                ..Default::default()
            },
            Successor::Condition(taken),
            (bank, block),
            |_, _, edits, (bank, block)| {
                debug_assert!(edits.is_empty());
                bank.publish();
                if let Some(block) = block {
                    block.publish();
                }
            },
        )?;

        Ok(published.then_some(taken))
    }
}
