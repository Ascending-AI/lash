//! Packet restoration adopts Incoming and actual saved owners in one event.
use super::*;
use crate::admission::Allowance;
use crate::compile::NestedKind;
use crate::machine::source_publication::Prefix;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, Program, PublicationValue as PV};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Phase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_call_resume(
        &mut self,
        frame: &mut Values<'_, '_>,
        continuation: CallContinuation<'_>,
        value: Value,
    ) -> Result<bool, HeapFault> {
        let site = continuation.site;
        if site.root.is_some() || self.projection().is_some() {
            return Ok(false);
        }
        let Some(act) = site.act else {
            return Ok(false);
        };
        let Some(id) = frame.state.statement else {
            return Ok(false);
        };
        let (stream, statement, _) = self.exe.statement(id);
        let entry = stream
            .nested
            .get(act.ix())
            .ok_or_else(|| malformed("packet restoration has no source entry"))?;
        let counted = entry.guest && matches!(entry.kind, NestedKind::Call(_));
        let body = matches!(entry.kind, NestedKind::Body(_));
        if !(counted || body) {
            return Ok(false);
        }
        let action = if counted {
            let Some(ordinal) = continuation.occurrence else {
                return Ok(false);
            };
            if self
                .counters
                .current(entry.occurrence)
                .is_none_or(|count| ordinal >= count)
            {
                return Err(malformed("packet restoration lost its admitted occurrence"));
            }
            Some((
                Point {
                    site: entry.site.clone(),
                    phase: Phase::Enter,
                },
                ordinal,
            ))
        } else {
            if continuation.occurrence.is_some() {
                return Err(malformed(
                    "uncounted packet restoration acquired an action ordinal",
                ));
            }
            None
        };
        let resume = statement
            .ops
            .start
            .checked_add(entry.resume)
            .ok_or_else(|| malformed("packet restoration coordinate overflow"))?;
        let Some(projected) = statement.source_roles_at(stream, resume) else {
            return Ok(false);
        };
        if site.reg != entry.dst.0
            || projected.len() != entry.roles.len() + 1
            || projected.iter().any(|(reg, at)| {
                !((*reg == entry.dst && at.as_ref() == &entry.site)
                    || entry
                        .roles
                        .iter()
                        .any(|role| role.reg == *reg && &role.site == at.as_ref()))
            })
        {
            return Err(malformed(
                "packet restoration differs from its source projection",
            ));
        }
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: entry.site.clone(),
            phase: Phase::Restore,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        let Ok(unpaid) = source.next_after_commit(&entry.site) else {
            return Ok(false);
        };
        self.logical.flush(self.arena, self.heap)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("packet restoration has no activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("packet restoration source owners failed verification"))?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let mut inputs = Vec::new();
        let mut outputs = Vec::new();
        let mut dependencies = Vec::new();
        let count = entry.roles.len() + 1;
        inputs
            .try_reserve(count)
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        outputs
            .try_reserve(count)
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        dependencies
            .try_reserve(count)
            .map_err(|_| HeapFault::Exhausted {
                resource: doc::heap::HostResource::StagingBytes,
            })?;
        for (at, value) in entry
            .roles
            .iter()
            .map(|role| (&role.site, frame.regs[role.reg.ix()]))
            .chain(std::iter::once((&entry.site, value)))
        {
            let input = if at == &entry.site {
                Owner::Incoming(self.task)
            } else {
                slots
                    .continuation(at, self.task, activation.invocation.id())
                    .ok_or_else(|| malformed("packet restoration has no continuation owner"))?
            };
            let output = slots
                .root(
                    &room::Owner::Operand(at.clone()),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("packet restoration has no operand owner"))?;
            if ledger.holding(input, self.heap.pages())?.is_none()
                || ledger.holding(output, self.heap.pages())?.is_some()
            {
                return Ok(false);
            }
            inputs.push(input);
            outputs.push((output, PV::Existing(input)));
            dependencies.extend(value.handle());
        }
        drop(ledger);
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.packet-call-resume");
        at.site.encode(&mut encoding);
        encoding.unsigned(site.pc as u64);
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
                action,
            },
        };
        let next = SourcePosition {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
        let program = Program::new(vec![JournalEvent {
            inputs,
            outputs,
            ..Default::default()
        }]);
        let output = &mut frame.regs[site.reg as usize];
        let published = self.publish_source(
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
            |_, edits| {
                debug_assert!(edits.is_empty());
                *output = value;
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::packet_call_resumed(entry.roles.len());
        }
        Ok(published)
    }
}
