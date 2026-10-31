//! A nested Leave releases only that scope's genuine source owners.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, Program};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
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
        let (unpaid, while_body) = match frame.state.control.iter().rev().nth(1) {
            Some(Position::Block(parent, next)) => {
                let parent = self
                    .exe
                    .block(frame.state.callee.compiled.blocks[*parent].id);
                let unpaid = if let Some(stmt) = parent.stmts.get(*next) {
                    match source.first_event(&self.exe.stmt(*stmt).site) {
                        Ok(point) => point,
                        Err(_) => return Ok(false),
                    }
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
                (unpaid, false)
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
                (unpaid, true)
            }
            _ => return Ok(false),
        };
        debug_assert!(!while_body || frame.state.control.len() >= 2);
        let leave = Point {
            site: closing.clone(),
            phase: SourcePhase::Leave,
        };
        let Ok(interval) = source.compile_event(&leave, ExitKind::Normal) else {
            return Ok(false);
        };
        self.source_bindings(frame)?;
        self.logical.flush(self.arena, self.heap)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("nested scope has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("nested scope owners failed verification"))?;
        let groups = vec![
            room::Owner::Scope(closing.clone()),
            room::Owner::Operands(closing.clone()),
        ];
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let mut inputs = BTreeSet::new();
        for selector in &groups {
            let owners = slots
                .group(selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("nested scope has no canonical owner group"))?;
            for owner in owners {
                if ledger.holding(owner, self.heap.pages())?.is_some() {
                    inputs.insert(owner);
                }
            }
        }
        let inputs = inputs.into_iter().collect::<Vec<_>>();
        let released = ledger.release_cost(&inputs, self.heap.pages())?;
        let facts = LeafExecution(
            [(
                leave.clone(),
                LeafFacts {
                    values: [(Measure::Release(groups.into()), i128::from(released))].into(),
                    ..Default::default()
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.nested-leave");
        closing.encode(&mut encoding);
        inputs.encode(&mut encoding);
        let start = SourcePosition {
            at: leave.clone(),
            unpaid: Some(leave.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: frame
                    .state
                    .control
                    .iter()
                    .filter_map(|control| match control {
                        Position::Loop { id, started, .. } => {
                            Some((self.exe.stmt(*id).site.clone(), started.saturating_sub(1)))
                        }
                        _ => None,
                    })
                    .collect(),
                action: None,
                departure: None,
            },
        };
        let next = SourcePosition {
            at: leave,
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: None,
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        let make = || {
            Program::new(vec![JournalEvent {
                inputs: inputs.clone(),
                ..Default::default()
            }])
        };
        drop(ledger);
        let declarations = &frame.state.callee.compiled.blocks[block].declares;
        let published = self.publish_source(
            frame.state.callee,
            activation,
            publication::Prefix {
                interval,
                facts,
                start,
                next,
                allowance,
            },
            make,
            |_, edits| {
                debug_assert!(edits.is_empty());
                for at in declarations {
                    frame.slots[*at as usize] = SlotState::Empty;
                }
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::scope_published();
            if while_body {
                crate::machine::islands::witness::while_scope_published(false);
            }
        }
        Ok(published)
    }
}
