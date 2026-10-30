//! Session writes transfer the actual RHS occurrence into its durable owner.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{JournalEvent, Program, PublicationValue as PV};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_session_write(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &sites::Publish,
        value: Value,
    ) -> Result<Option<bool>, HeapFault> {
        if self.tape.is_some() || self.projection().is_some() {
            return Ok(None);
        }
        let Some(binding) = self.stream.names.get(site.name as usize).cloned() else {
            return Ok(None);
        };
        if !site.bind && self.session.resident(&binding.key).is_none() {
            return Ok(None);
        }
        let Some(statement) = frame.state.statement else {
            return Ok(None);
        };
        let (stream, statement, _) = self.exe.statement(statement);
        let Some(at) = stream
            .source_sites
            .get(site.pc as usize)
            .and_then(Option::as_ref)
        else {
            return Ok(None);
        };
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        if source.site.unit != lash_kernel_value::sites::Unit::Main {
            return Ok(None);
        }
        match stream.ops.get(site.pc as usize) {
            Some(lash_kernel_exec::Op::BindSession { name, src })
                if site.bind && name.0 == site.name && src.0 == site.reg => {}
            Some(lash_kernel_exec::Op::WriteSession { name, src })
                if !site.bind && name.0 == site.name && src.0 == site.reg => {}
            _ => return Ok(None),
        }
        if site.bind && source.persistent.get(at) != Some(&binding.name) {
            return Ok(None);
        }
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(None);
        };
        let Some((_, rhs)) = roles.iter().find(|(reg, _)| reg.0 == site.reg) else {
            return Ok(None);
        };
        let Some(unpaid) = self.source_next_statement(frame) else {
            return Ok(None);
        };
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(None);
        };
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("session write has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("session source owners failed verification"))?;
        let input = slots
            .root(
                &room::Owner::Operand((**rhs).clone()),
                self.task,
                activation.invocation.id(),
            )
            .ok_or_else(|| malformed("session RHS has no source owner"))?;
        let output = if site.bind {
            room::Owner::SessionCell {
                at: at.site.clone(),
                name: binding.name.clone(),
            }
        } else {
            room::Owner::Visible {
                at: at.site.clone(),
                name: binding.name.clone(),
            }
        };
        let Some(output) = slots.root(&output, self.task, activation.invocation.id()) else {
            return Ok(None);
        };
        if output != Owner::session(&binding.name) {
            return Ok(None);
        }
        self.logical.replace(input, Some(SourceValue::Value(value)));
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        let held = ledger.holding(output, self.heap.pages())?.is_some();
        if !site.bind && !held {
            return Ok(None);
        }
        let mut inputs = vec![input];
        let released = if held {
            inputs.push(output);
            ledger.release_cost(&[output], self.heap.pages())?
        } else {
            0
        };
        let mut measures = std::collections::BTreeMap::new();
        let event = interval
            .events()
            .next()
            .ok_or_else(|| malformed("session event disappeared"))?;
        for measure in event.delta.terms.keys() {
            if matches!(measure, Measure::Release(_)) {
                measures.insert(measure.clone(), i128::from(released));
            }
        }
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: value.handle().into_iter().collect(),
                    values: measures,
                    ..Default::default()
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.session-write");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
        output.encode(&mut encoding);
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
            at,
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
                outputs: vec![(output, PV::Existing(input))],
                ..Default::default()
            }])
        };
        drop(ledger);
        let storage = self.session.prepare_write(&binding, value)?;
        if !self
            .scratch
            .sessions
            .iter()
            .any(|(name, _)| *name == site.name)
        {
            self.scratch
                .sessions
                .try_reserve(1)
                .map_err(|_| HeapFault::Exhausted {
                    resource: doc::heap::HostResource::StagingBytes,
                })?;
        }
        let published = self.publish_source_prepared(
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
            publication::Reserve(|_: &mut Arena| Ok(Some(storage))),
            |_, session, edits, storage| {
                debug_assert!(edits.is_empty());
                storage.publish(session);
            },
        )?;
        if published {
            match self
                .scratch
                .sessions
                .iter_mut()
                .find(|(name, _)| *name == site.name)
            {
                Some((_, cached)) => *cached = value,
                None => self.scratch.sessions.push((site.name, value)),
            }
            #[cfg(any(test, feature = "testing"))]
            crate::machine::islands::witness::session_published();
        }
        Ok(Some(published))
    }
}
