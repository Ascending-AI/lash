//! Direct field and index reads publish before their result can become a saved role.
use super::*;
use crate::admission::Allowance;
use crate::machine::source_publication::Prefix;
use lash_kernel_state::kernel_internals::logical::{Holding, PublicationValue, RootValue};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase, Point,
    publication::{ExactRoots, Occurrence, Position as SourcePosition, Successor},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_member_read(
        &mut self,
        frame: &mut Values<'_, '_>,
        pc: u32,
        reg: u32,
        value: Value,
    ) -> Result<bool, HeapFault> {
        let Some(id) = frame.state.statement else {
            return Ok(false);
        };
        if frame
            .state
            .source
            .as_ref()
            .is_none_or(|source| source.original.is_some())
        {
            return Ok(false);
        }
        let (stream, statement, _) = self.exe.statement(id);
        let Some(op @ (lash_kernel_exec::Op::Field { .. } | lash_kernel_exec::Op::Index { .. })) =
            stream.ops.get(pc as usize)
        else {
            return Ok(false);
        };
        if op.writes().is_none_or(|dst| dst.0 != reg) {
            return Err(malformed("packet member read has another source output"));
        }
        let Some(Some(site)) = stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        let Some(roles) = statement.source_roles_at(stream, pc) else {
            return Ok(false);
        };
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: (**site).clone(),
            phase: Phase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        let Ok(unpaid) = source.next_after_commit(site) else {
            return Ok(false);
        };
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("member read source owners failed verification"))?;
        self.logical.flush(self.arena, self.heap)?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("member read has no source invocation"))?;
        let root = |site| {
            slots
                .root(
                    &room::Owner::Operand(site),
                    self.task,
                    activation.invocation.id(),
                )
                .ok_or_else(|| malformed("member read has no canonical source owner"))
        };
        let output = root(at.site.clone())?;
        let mut inputs = Vec::new();
        let mut dependencies = value.handle().into_iter().collect::<Vec<_>>();
        for reg in op.reads() {
            let Some((_, site)) = roles.iter().find(|(actual, _)| actual == &reg) else {
                return Ok(false);
            };
            inputs.push(root((**site).clone())?);
            dependencies.extend(frame.regs[reg.ix()].handle());
        }
        let holding = Holding::from_native(self.arena, self.heap.pages(), &value)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        for input in &inputs {
            if ledger.holding(*input, self.heap.pages())?.is_none() {
                return Ok(false);
            }
        }
        let released = ledger.release_cost_preserving(
            self.arena,
            self.heap,
            &inputs,
            &[(output, RootValue::Value(&value))],
        )?;
        drop(ledger);
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("member read event disappeared"))?
            .inputs
            .clone();
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.packet-member-read");
        at.site.encode(&mut encoding);
        output.encode(&mut encoding);
        let start = SourcePosition {
            at: at.clone(),
            unpaid: Some(at.clone()),
            recovery: logical_digest(&encoding.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: self.source_loops(&frame.state.control),
                action: None,
            },
        };
        let next = SourcePosition {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let safe_point = self.charged;
        let destination = &mut frame.regs[reg as usize];
        self.publish_source_event(
            frame.state.callee,
            activation,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at.clone(),
                        LeafFacts {
                            dependencies,
                            values: [
                                (
                                    Measure::Local(room::Owner::Operand(at.site.clone())),
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
            (),
            |_, _, edits, ()| {
                debug_assert!(edits.is_empty());
                *destination = value;
            },
        )
    }
}
