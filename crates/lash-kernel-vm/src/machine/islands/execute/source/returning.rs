//! A direct return transfers its real RHS, then leaves the same Invocation.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{
    FrameEvent, Holding, JournalEvent, Program, PublicationValue as PV, RootValue,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_return_suffix(
        &mut self,
        frame: &mut Values<'_, '_>,
        value: Value,
    ) -> Result<ValuePublication, HeapFault> {
        if !self.source_return_frame(frame)
            || self.tape.is_some()
            || self.projection().is_some()
            || frame
                .state
                .source
                .as_ref()
                .and_then(|source| source.frontier.as_ref())
                .is_some_and(|frontier| frontier.position().unpaid.is_none())
        {
            return Ok(ValuePublication::Unavailable);
        }
        let Some(returned) = frame.state.returned else {
            return Ok(ValuePublication::Unavailable);
        };
        if frame.state.statement != Some(returned.statement) {
            return Ok(ValuePublication::Unavailable);
        }
        let (stream, statement, _) = self.exe.statement(returned.statement);
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: statement.site.clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_return_suffix(&at) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some(roles) = statement.source_roles_at(stream, returned.pc) else {
            return Ok(ValuePublication::Unavailable);
        };
        let Some((_, rhs)) = roles.iter().find(|(reg, _)| reg.0 == returned.reg) else {
            return Ok(ValuePublication::Unavailable);
        };
        self.source_bindings(frame)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("return has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("return source owners failed verification"))?;
        let root = |selector| {
            slots
                .root(&selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("return has no canonical source owner"))
        };
        let input_selector = room::Owner::Operand((**rhs).clone());
        let departure_selector = room::Owner::Departure(at.site.clone());
        let input = root(input_selector.clone())?;
        let departure = root(departure_selector.clone())?;
        let incoming = Owner::Incoming(self.task);
        self.logical.replace(input, Some(SourceValue::Value(value)));
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(departure, self.heap.pages())?.is_some()
            || ledger.holding(incoming, self.heap.pages())?.is_some()
        {
            return Ok(ValuePublication::Declined);
        }
        let bytes = Holding::from_native(self.arena, self.heap.pages(), &value)?.bytes();
        let released = ledger.release_cost_preserving(
            self.arena,
            self.heap,
            &[input],
            &[(departure, RootValue::Existing(input))],
        )?;
        let returning = JournalEvent {
            inputs: vec![input],
            outputs: vec![(departure, PV::Existing(input))],
            ..Default::default()
        };
        let preview = Program::new(vec![returning.clone()]);
        let Some(preview) = preview.evaluate(&ledger, self.arena) else {
            return Ok(ValuePublication::Declined);
        };
        let preview = preview
            .prepare(
                &ledger,
                self.arena
                    .registration()
                    .map_err(publication::native_fault)?,
                self.heap,
                Program::new(vec![returning.clone()]),
            )
            .map_err(super::unit::member_fault)?;
        let groups = vec![
            room::Owner::Scope(source.site.clone()),
            room::Owner::Operands(source.site.clone()),
        ];
        let mut closing = std::collections::BTreeSet::new();
        for group in &groups {
            for owner in slots
                .group(group, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("return scope has no canonical owner group"))?
            {
                if owner != input && ledger.holding(owner, self.heap.pages())?.is_some() {
                    closing.insert(owner);
                }
            }
        }
        let closing = closing.into_iter().collect::<Vec<_>>();
        let release_scope = preview.release_cost(&closing, self.heap.pages())?;
        drop(preview);
        let leave = Point {
            site: source.site.clone(),
            phase: SourcePhase::Leave,
        };
        let facts = LeafExecution(
            [
                (
                    at.clone(),
                    LeafFacts {
                        dependencies: value.handle().into_iter().collect(),
                        values: [
                            (Measure::Local(departure_selector), i128::from(bytes)),
                            (
                                Measure::Release(vec![input_selector].into()),
                                i128::from(released),
                            ),
                        ]
                        .into(),
                        ..Default::default()
                    },
                ),
                (
                    leave.clone(),
                    LeafFacts {
                        values: [(Measure::Release(groups.into()), i128::from(release_scope))]
                            .into(),
                        ..Default::default()
                    },
                ),
            ]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(4);
        encoding.text("lash.source.return-leave");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
        departure.encode(&mut encoding);
        let start = SourcePosition {
            at: at.clone(),
            unpaid: Some(at),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: vec![],
                action: None,
                departure: None,
            },
        };
        let next = SourcePosition {
            at: leave,
            unpaid: None,
            ..start.clone()
        };
        let task = self.task;
        let invocation = activation.invocation.id();
        let make = || {
            Program::new(vec![
                returning.clone(),
                JournalEvent {
                    frame: Some(FrameEvent::Leave { task, invocation }),
                    outputs: vec![(incoming, PV::Existing(departure))],
                    ..Default::default()
                },
            ])
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
        drop(ledger);
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
            |_, edits| debug_assert!(edits.is_empty()),
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            crate::machine::islands::witness::return_published();
        }
        Ok(if published {
            ValuePublication::Published(value)
        } else {
            ValuePublication::Declined
        })
    }
}
