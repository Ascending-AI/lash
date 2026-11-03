//! A nested Leave releases only that scope's genuine source owners.
use super::*;
use crate::admission::Allowance;
use crate::machine::source_publication::Prefix;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{ExactRoots, Occurrence, Position as SourcePosition, Successor},
};
use std::collections::BTreeSet;

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_nested_leave(
        &mut self,
        frame: &mut Values<'_, '_>,
        block: usize,
    ) -> Result<bool, HeapFault> {
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let closing = &self
            .exe
            .block(frame.state.callee.compiled.blocks[block].id)
            .site;
        if closing == &source.site {
            return Ok(false);
        }
        // A parent position with no next source event refuses, as the checked
        // path does.
        let (loop_body, dependency, unpaid) = match frame.state.control.iter().rev().nth(1) {
            Some(Position::Block(parent, next)) => {
                let parent = self
                    .exe
                    .block(frame.state.callee.compiled.blocks[*parent].id);
                let unpaid = if let Some(stmt) = parent.stmts.get(*next) {
                    let Ok(point) = source.first_event(&self.exe.stmt(*stmt).site) else {
                        return Ok(false);
                    };
                    point
                } else {
                    Point {
                        site: parent.site.clone(),
                        phase: if parent.site == source.site {
                            SourcePhase::Fallthrough
                        } else {
                            SourcePhase::Leave
                        },
                    }
                };
                (None, None, unpaid)
            }
            Some(Position::Loop {
                id,
                started,
                cursor: None,
                ..
            }) => {
                let crate::compile::Form::While { site, body, .. } = &self.exe.stmt(*id).form
                else {
                    return Err(malformed("scope parent has another loop form"));
                };
                if *body != frame.state.callee.compiled.blocks[block].id || *started == 0 {
                    return Err(malformed("scope is not an entered while body"));
                }
                let Ok(unpaid) = source.first_event(&site.child(0)) else {
                    return Ok(false);
                };
                (Some(ScopeLoop::While), None, unpaid)
            }
            Some(Position::Loop {
                id,
                started,
                cursor: Some(cursor),
                guard: None,
                ..
            }) => {
                let crate::compile::Form::For {
                    site,
                    body,
                    via: None,
                    ..
                } = &self.exe.stmt(*id).form
                else {
                    return Ok(false);
                };
                if *body != frame.state.callee.compiled.blocks[block].id || *started == 0 {
                    return Err(malformed("scope is not an entered for body"));
                }
                let Some(Ok((_, next))) = cursor.resident_step(self.arena) else {
                    return Ok(false);
                };
                let continued = next.is_some();
                let Ok(unpaid) = source.next_after_for_cursor(site, continued) else {
                    return Ok(false);
                };
                (
                    Some(ScopeLoop::For { continued }),
                    cursor.walked().and_then(|value| value.handle()),
                    unpaid,
                )
            }
            _ => return Ok(false),
        };
        debug_assert!(loop_body.is_none() || frame.state.control.len() >= 2);
        debug_assert!(!matches!(loop_body, Some(ScopeLoop::For { .. })) || dependency.is_some());
        let at = Point {
            site: closing.clone(),
            phase: SourcePhase::Leave,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        if !interval.scope_release(source, closing) {
            return Ok(false);
        }
        self.logical.flush(self.arena, self.heap)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("nested scope has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("scope owners failed verification"))?;
        let groups = vec![
            room::Owner::Scope(closing.clone()),
            room::Owner::Operands(closing.clone()),
        ];
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let live = ledger.invocation_owners(&activation.invocation, self.heap.pages())?;
        let members = frame.state.callee.compiled.blocks[block]
            .leave
            .get_or_init(|| {
                let mut members = BTreeSet::new();
                for group in &groups {
                    for owner in slots.group(group, self.task, 0)? {
                        let Owner::Value { slot, .. } = owner else {
                            return None;
                        };
                        members.insert(slot);
                    }
                }
                Some(members.into_iter().collect())
            });
        let Some(members) = members else {
            return Ok(false);
        };
        let inputs: Vec<_> = live
            .into_iter()
            .filter(|owner| {
                matches!(owner,
            Owner::Value { slot, .. } if members.binary_search(slot).is_ok())
            })
            .collect();
        let released = ledger.release_cost(&inputs, self.heap.pages())?;
        drop(ledger);
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.packet-scope-leave");
        at.site.encode(&mut encoding);
        inputs.encode(&mut encoding);
        let start = SourcePosition {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: frame
                    .state
                    .control
                    .iter()
                    .enumerate()
                    .filter_map(|(index, control)| match control {
                        Position::Loop {
                            id,
                            started,
                            cursor,
                            ..
                        } => Some((
                            self.exe.stmt(*id).site.clone(),
                            started.saturating_sub(u64::from(
                                cursor.is_some() || index + 1 < frame.state.control.len(),
                            )),
                        )),
                        _ => None,
                    })
                    .collect(),
                action: None,
            },
        };
        let next = SourcePosition {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let declared = &frame.state.callee.compiled.blocks[block].declares;
        let published = self.publish_source_event(
            frame.state.callee,
            activation,
            Prefix {
                interval,
                start,
                next,
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
                allowance: Allowance {
                    occurrences: vec![],
                    before: 0,
                    units: 0,
                    close: None,
                    strict: false,
                    depth: 0,
                    safe_point: self.charged,
                },
            },
            ExactRoots {
                inputs,
                ..Default::default()
            },
            match loop_body {
                Some(ScopeLoop::For { continued }) => Successor::Cursor(continued),
                _ => Successor::Normal,
            },
            &mut frame.slots,
            |_, _, edits, slots| {
                assert!(edits.is_empty());
                for at in declared {
                    slots[*at as usize] = SlotState::Empty;
                }
            },
        )?;
        if !published {
            return Ok(false);
        }
        #[cfg(any(test, feature = "testing"))]
        {
            crate::machine::islands::witness::scope_published();
            match loop_body {
                Some(ScopeLoop::While) => {
                    crate::machine::islands::witness::while_scope_published(false)
                }
                Some(ScopeLoop::For { continued }) => {
                    crate::machine::islands::witness::for_scope_published(false, continued)
                }
                None => {}
            }
        }
        Ok(true)
    }
}
