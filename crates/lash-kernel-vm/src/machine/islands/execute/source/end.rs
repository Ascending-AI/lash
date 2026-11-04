//! A packet discard retires its real RHS at the original source End.
use super::*;
use crate::admission::Allowance;
use crate::machine::source_publication::Prefix;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase, Point,
    publication::{ExactRoots, Occurrence, Position as SourcePosition, Successor},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_discard(
        &mut self,
        frame: &mut Values<'_, '_>,
        pc: u32,
    ) -> Result<bool, HeapFault> {
        let Some(id) = frame.state.statement else {
            return Ok(false);
        };
        let (stream, statement, _) = self.exe.statement(id);
        if !matches!(stream.ops.get(pc as usize), Some(lash_kernel_exec::Op::End)) {
            return Ok(false);
        }
        let Some(Some(site)) = stream.source_sites.get(pc as usize) else {
            return Ok(false);
        };
        if site.as_ref() != &statement.site {
            return Err(malformed("packet End names another source statement"));
        }
        let rhs = site.child(0);
        let Some(roles) = statement.source_roles_at(stream, pc) else {
            return Ok(false);
        };
        let [(reg, role)] = roles.as_ref() else {
            return Ok(false);
        };
        if role.as_ref() != &rhs {
            return Ok(false);
        }
        let value = frame.regs[reg.ix()];
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let Some(unpaid) = self.source_next_statement(frame) else {
            return Ok(false);
        };
        let at = Point {
            site: statement.site.clone(),
            phase: Phase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        let Some(activation) = frame.state.source.as_mut() else {
            return Ok(false);
        };
        // The original receipt still owns its unpaid fixed suffix. Precise
        // source consumes that receipt before this ordinary zero-debt End.
        if activation.original.is_some() {
            return Ok(false);
        }
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("packet End owners failed verification"))?;
        let input = slots
            .root(
                &room::Owner::Operand(rhs),
                self.task,
                activation.invocation.id(),
            )
            .ok_or_else(|| malformed("packet End has no canonical RHS owner"))?;
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let store = self.logical.clone();
        let ledger = store.borrow();
        if ledger.holding(input, self.heap.pages())?.is_none() {
            return Ok(false);
        }
        let released = ledger.release_cost(&[input], self.heap.pages())?;
        drop(ledger);
        let release = interval
            .events()
            .next()
            .ok_or_else(|| malformed("packet End event disappeared"))?
            .inputs
            .clone();
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.packet-end");
        at.site.encode(&mut encoding);
        input.encode(&mut encoding);
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
            },
        };
        let next = SourcePosition {
            unpaid: Some(unpaid),
            ..start.clone()
        };
        self.publish_source_event(
            frame.state.callee,
            activation,
            Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at,
                        LeafFacts {
                            dependencies: value.handle().into_iter().collect(),
                            values: [(Measure::Release(release), i128::from(released))].into(),
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
                    safe_point: self.charged,
                },
            },
            ExactRoots {
                inputs: vec![input],
                ..Default::default()
            },
            Successor::Normal,
            (),
            |_, _, edits, ()| assert!(edits.is_empty()),
        )
    }
}
