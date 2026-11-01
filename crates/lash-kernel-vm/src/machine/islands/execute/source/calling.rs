//! A packet call borrows its actual operands and admits its occurrence together.
use super::*;
use crate::admission::Allowance;
use crate::compile::NestedKind;
use crate::machine::source_publication::Prefix;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, Program};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Phase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_call_enter(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &compiled::CallSite,
        args: &[u32],
        index: usize,
    ) -> Result<Option<u64>, Stop> {
        if site.root.is_some() || self.projection().is_some() {
            return Ok(None);
        }
        let Some(act) = site.act else { return Ok(None) };
        let Some(id) = frame.state.statement else {
            return Ok(None);
        };
        let (stream, statement, _) = self.exe.statement(id);
        let entry = stream
            .nested
            .get(act.ix())
            .ok_or_else(|| self.halt(malformed("packet call has no nested source").into()))?;
        if !entry.guest || !matches!(entry.kind, NestedKind::Call(_)) {
            return Ok(None);
        }
        if entry.occurrence != index {
            return Err(self.halt(malformed("packet call names another occurrence").into()));
        }
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: entry.site.clone(),
            phase: Phase::Enter,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Action) else {
            return Ok(None);
        };
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(None);
        };
        self.logical
            .flush(self.arena, self.heap)
            .map_err(|error| self.halt(error.into()))?;
        let activation =
            frame.state.source.as_mut().ok_or_else(|| {
                self.halt(malformed("packet call has no source activation").into())
            })?;
        let slots = source.owner_slots().map_err(|_| {
            self.halt(malformed("packet call source owners failed verification").into())
        })?;
        let invocation = activation.invocation.id();
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let mut borrowed = Vec::new();
        let mut dependencies = Vec::new();
        for selector in &interval
            .events()
            .next()
            .ok_or_else(|| self.halt(malformed("packet call Enter disappeared").into()))?
            .inputs
        {
            let room::Owner::Operand(at) = selector else {
                return Err(self.halt(malformed("packet call has another input class").into()));
            };
            let Some((reg, _)) = roles.iter().find(|(_, role)| role.as_ref() == at) else {
                return Ok(None);
            };
            if !args.contains(&reg.0) && reg.0 != site.reg {
                return Err(self.halt(malformed("packet call borrows another operand").into()));
            }
            let owner = slots.root(selector, self.task, invocation).ok_or_else(|| {
                self.halt(malformed("packet argument has no source owner").into())
            })?;
            if ledger
                .holding(owner, self.heap.pages())
                .map_err(|error| self.halt(error.into()))?
                .is_none()
            {
                return Ok(None);
            }
            borrowed.push(owner);
            dependencies.extend(frame.regs[reg.ix()].handle());
        }
        drop(ledger);
        let ordinal = self
            .counters
            .current(index)
            .ok_or_else(|| self.halt(fault("unknown packet call occurrence")))?;
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.packet-call-enter");
        at.site.encode(&mut encoding);
        encoding.seq(borrowed.len() as u64);
        for owner in &borrowed {
            owner.encode(&mut encoding);
        }
        let start = SourcePosition {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation,
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
                action: Some((at.clone(), ordinal)),
            },
        };
        let next = SourcePosition {
            unpaid: Some(Point {
                site: entry.site.clone(),
                phase: Phase::Commit,
            }),
            ..start.clone()
        };
        let safe_point = self.charged;
        let program = Program::new(vec![JournalEvent {
            borrowed,
            ..Default::default()
        }]);
        let published = self
            .publish_source(
                frame.state.callee,
                activation,
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
                        occurrences: vec![index],
                        before: 0,
                        units: 0,
                        close: None,
                        strict: false,
                        depth: 1,
                        safe_point,
                    },
                },
                || program.clone(),
                |_, edits| debug_assert!(edits.is_empty()),
            )
            .map_err(|error| self.halt(error.into()))?;
        if !published {
            return Err(self.at(site.pc));
        }
        #[cfg(any(test, feature = "testing"))]
        super::super::super::witness::packet_call_entered();
        Ok(Some(ordinal))
    }
}
