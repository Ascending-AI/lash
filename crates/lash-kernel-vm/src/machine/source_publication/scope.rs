//! Normal nested block exits publish their owner release and reserved clears.
use super::*;
use crate::compile::room::publication::{ExactRoots, Successor};
use crate::compile::{
    Executable,
    room::{ExitKind, Measure, Owner as Selector, Phase, Point, TryScope, publication::Occurrence},
};
use crate::machine::{Completion, Control, Halt, KernelMachine, TryPhase, fault};

use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use std::collections::BTreeSet;

impl KernelMachine {
    pub(in crate::machine) fn publish_source_scope_leave(
        &mut self,
        task_id: doc::TaskId,
        exe: &Executable,
    ) -> Result<bool, Halt> {
        let arena = std::rc::Rc::clone(&self.arena);
        let (source, link, loops, block, invocation, unpaid, loop_body, dependency, try_scope) = {
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
            let try_scope = match frame.control.iter().rev().nth(1) {
                Some(Control::Try {
                    phase: TryPhase::Body,
                    ..
                }) => Some(TryScope::Body),
                Some(Control::Try {
                    phase: TryPhase::Catch,
                    ..
                }) => Some(TryScope::Catch),
                Some(Control::Try {
                    phase: TryPhase::Finally(Completion::Normal),
                    ..
                }) => Some(TryScope::Finally),
                _ => None,
            };
            let (unpaid, loop_body, dependency) = match frame.control.iter().rev().nth(1) {
                Some(Control::Block {
                    block: parent,
                    next,
                }) => {
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
                    (unpaid, None, None)
                }
                Some(Control::While { stmt, started }) => {
                    let crate::compile::Form::While { site, body, .. } = &exe.stmt(*stmt).form
                    else {
                        return Err(malformed("scope parent has another loop form").into());
                    };
                    if body != block || *started == 0 {
                        return Err(malformed("scope is not an entered while body").into());
                    }
                    let Ok(unpaid) = source.first_event(&site.child(0)) else {
                        return Ok(false);
                    };
                    (unpaid, Some(ScopeLoop::While), None)
                }
                Some(Control::For {
                    stmt,
                    cursor,
                    started,
                    header: crate::machine::loops::Header::Body,
                    guard: None,
                }) => {
                    let crate::compile::Form::For {
                        site,
                        body,
                        via: None,
                        ..
                    } = &exe.stmt(*stmt).form
                    else {
                        return Ok(false);
                    };
                    if body != block || *started == 0 {
                        return Err(malformed("scope is not an entered for body").into());
                    }
                    let Some(Ok((_, next))) = cursor.resident_step(&arena.borrow()) else {
                        return Ok(false);
                    };
                    let continued = next.is_some();
                    let Ok(unpaid) = source.next_after_for_cursor(site, continued) else {
                        return Ok(false);
                    };
                    (
                        unpaid,
                        Some(ScopeLoop::For { continued }),
                        cursor.walked().and_then(|value| value.handle()),
                    )
                }
                Some(Control::Try {
                    stmt,
                    phase: TryPhase::Body | TryPhase::Catch | TryPhase::Finally(Completion::Normal),
                }) => {
                    let crate::compile::Form::Try {
                        body,
                        catch,
                        finally,
                        ..
                    } = &exe.stmt(*stmt).form
                    else {
                        return Err(malformed("scope parent has another try form").into());
                    };
                    let scope =
                        try_scope.ok_or_else(|| malformed("scope has no normal try phase"))?;
                    let expected = match scope {
                        TryScope::Body => body,
                        TryScope::Catch => catch
                            .as_ref()
                            .map(|(_, block)| block)
                            .ok_or_else(|| malformed("catch scope has no source catch"))?,
                        TryScope::Finally => finally
                            .as_ref()
                            .ok_or_else(|| malformed("finally scope has no source cleanup"))?,
                    };
                    if expected != block {
                        return Err(malformed("scope is not the entered try block").into());
                    }
                    let Ok(unpaid) = source.next_after_try_scope(&exe.stmt(*stmt).site, scope)
                    else {
                        return Ok(false);
                    };
                    (unpaid, None, None)
                }
                _ => return Ok(false),
            };
            (
                source,
                frame.link,
                loops,
                *block,
                roots.invocation().id(),
                unpaid,
                loop_body,
                dependency,
                try_scope,
            )
        };
        debug_assert!(try_scope.is_none() || loop_body.is_none());
        debug_assert!(loop_body.is_none() || !loops.is_empty());
        debug_assert!(!matches!(loop_body, Some(ScopeLoop::For { .. })) || dependency.is_some());
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
        let live = ledger.frame_owners(task_id, invocation, self.heap.pages())?;
        for group in &groups {
            for owner in owners
                .select_group(group, task_id, invocation, &live)
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
                        at,
                        LeafFacts {
                            dependencies: dependency.into_iter().collect(),
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
            ExactRoots {
                inputs,
                outputs: vec![],
                ..Default::default()
            },
            match loop_body {
                Some(ScopeLoop::For { continued }) => Successor::Cursor(continued),
                _ => Successor::Normal,
            },
            (bank, slots, controls),
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
            match try_scope {
                Some(TryScope::Body) => crate::machine::islands::witness::precise_try_scope(false),
                Some(TryScope::Catch) => crate::machine::islands::witness::precise_try_scope(true),
                Some(TryScope::Finally) => {
                    crate::machine::islands::witness::precise_try_finally_scope()
                }
                None => {}
            }

            match loop_body {
                Some(ScopeLoop::While) => {
                    crate::machine::islands::witness::while_scope_published(true)
                }
                Some(ScopeLoop::For { continued }) => {
                    crate::machine::islands::witness::for_scope_published(true, continued)
                }
                None => {}
            }
        }
        Ok(published)
    }
}
