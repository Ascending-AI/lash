//! Normal nested block exits publish their owner release and reserved clears.
use super::*;
use crate::compile::{
    Executable,
    room::{ExitKind, Measure, Owner as Selector, Phase, Point, publication::Occurrence},
};
use crate::machine::{Control, Halt, KernelMachine, fault};
use lash_kernel_state::kernel_internals::logical::JournalEvent;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use std::collections::BTreeSet;

impl KernelMachine {
    pub(in crate::machine) fn publish_source_scope_leave(
        &mut self,
        task_id: doc::TaskId,
        exe: &Executable,
    ) -> Result<bool, Halt> {
        let (source, link, loops, block, invocation, unpaid) = {
            let frame = self.frame(task_id)?;
            let Some(roots) = &frame.logical else {
                return Ok(false);
            };
            let Some(loops) = source_loops(frame, exe)? else {
                return Ok(false);
            };
            let Some(Control::Block {
                block,
                next: completed,
            }) = frame.control.last()
            else {
                return Ok(false);
            };
            if *completed != exe.block(*block).stmts.len() {
                return Ok(false);
            }
            let source = Arc::clone(&exe.code(frame.code).source_room);
            if exe.block(*block).site == source.site {
                return Ok(false);
            }
            let Some(Control::Block {
                block: parent,
                next,
            }) = frame.control.iter().rev().nth(1)
            else {
                return Ok(false);
            };
            let parent = exe.block(*parent);
            if *next > parent.stmts.len() {
                return Err(malformed("scope parent has another source cursor").into());
            }
            let unpaid = if let Some(stmt) = parent.stmts.get(*next) {
                let Ok(point) = source.first_event(&exe.stmt(*stmt).site) else {
                    return Ok(false);
                };
                point
            } else {
                Point {
                    site: parent.site.clone(),
                    phase: if parent.site == source.site {
                        Phase::Fallthrough
                    } else {
                        Phase::Leave
                    },
                }
            };
            (
                source,
                frame.link,
                loops,
                *block,
                roots.invocation().id(),
                unpaid,
            )
        };
        let at = Point {
            site: exe.block(block).site.clone(),
            phase: Phase::Leave,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        self.logical_flush_controls(task_id, exe)?;
        self.logical_flush()?;
        let groups = vec![
            Selector::Scope(at.site.clone()),
            Selector::Operands(at.site.clone()),
        ];
        let owners = source
            .owner_slots()
            .map_err(|_| malformed("scope owners failed verification"))?;
        let mut inputs = BTreeSet::new();
        let store = self.logical.ledger();
        let ledger = store.borrow();
        for group in &groups {
            for owner in owners
                .group(group, task_id, invocation)
                .ok_or_else(|| malformed("scope has no canonical owner group"))?
            {
                if ledger.holding(owner, self.heap.pages())?.is_some() {
                    inputs.insert(owner);
                }
            }
        }
        let inputs = inputs.into_iter().collect::<Vec<_>>();
        let released = ledger.release_cost(&inputs, self.heap.pages())?;
        drop(ledger);
        let bank = self.regs.prepare_source_releases(&inputs)?;
        let document = exe
            .instances
            .get(&link)
            .ok_or_else(|| malformed("source link has no admitted document"))?;
        let kernel = doc::KernelVersion::of(document.kernel())
            .ok_or_else(|| malformed("source kernel is unsupported"))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.precise-scope-leave");
        at.site.encode(&mut encoding);
        inputs.encode(&mut encoding);
        let mut arena = self.arena.borrow_mut();
        let task = self
            .tasks
            .get_mut(task_id.0 as usize)?
            .ok_or_else(|| fault("no task"))?;
        let depth = (task.frames.len()
            + usize::from(task_id != doc::TaskId::MAIN || !self.session_cell))
            as u64;
        let frame = task.frames.last_mut()?.ok_or_else(|| fault("no frame"))?;
        let roots = frame
            .logical
            .as_ref()
            .ok_or_else(|| malformed("scope has no entered invocation"))?;
        if roots.invocation().id() != invocation
            || !matches!(frame.control.last(), Some(Control::Block { block: actual, .. }) if *actual == block)
        {
            return Err(malformed("scope preparation names another control").into());
        }
        let slots = frame.slots.prepare_scope(&exe.block(block).declares)?;
        let controls = &mut frame.control;
        let start = Position {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: task_id,
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
                            values: [(Measure::Release(groups.into()), i128::from(released))]
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
            || {
                Program::new(vec![JournalEvent {
                    inputs: inputs.clone(),
                    ..Default::default()
                }])
            },
            Reserve(move |_: &mut Arena| Ok(Some((bank, slots, controls)))),
            |_, _, edits, (bank, slots, controls)| {
                debug_assert!(edits.is_empty());
                let Some(Control::Block { block: actual, .. }) = controls.pop() else {
                    unreachable!("a prepared scope is a block")
                };
                assert_eq!(actual, block);
                slots.publish();
                bank.publish();
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::precise_scope_published();
        }
        Ok(published)
    }
}
