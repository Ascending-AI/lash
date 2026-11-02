//! A source member commit consumes its evaluated operand occurrences once.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{Member, MemberEdit};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_list_store(
        &mut self,
        frame: &mut Values<'_, '_>,
        pc: u32,
        operands: [(u32, Value); 3],
    ) -> Result<bool, HeapFault> {
        let [(_, target), (_, index), (_, value)] = operands;
        let Value::List(handle) = target else {
            return Ok(false);
        };
        let Some(index) = crate::compile::position(&index) else {
            return Ok(false);
        };
        // An out-of-range source write raises its guest error on the precise
        // path. It cannot be submitted as a malformed ownership transition.
        let Ok(len) = self.arena.len(handle) else {
            return Ok(false);
        };
        if index > len {
            return Ok(false);
        }
        self.publish_member_store(
            frame,
            pc,
            &operands,
            MemberEdit {
                handle,
                member: Member::List(index),
                next: Some(value),
            },
            None,
        )
    }

    pub(in crate::machine::islands::execute) fn publish_cell_store(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &crate::machine::islands::compiled::WriteSite,
        value: Value,
    ) -> Result<Option<bool>, HeapFault> {
        if site.bind {
            return Ok(None);
        }
        let SlotState::Cell(handle) = frame.slots[site.at] else {
            return Ok(None);
        };
        let Some(statement) = frame.state.statement else {
            return Ok(None);
        };
        let (_, statement, _) = self.exe.statement(statement);
        if !self
            .exe
            .code(frame.state.callee.plan.code)
            .source_room
            .assigns_cell(&statement.site)
        {
            return Ok(None);
        }
        self.publish_member_store(
            frame,
            site.pc,
            &[(site.reg, value)],
            MemberEdit {
                handle,
                member: Member::Variable,
                next: Some(value),
            },
            self.accounting.then_some(site.close).flatten(),
        )
        .map(Some)
    }

    fn publish_member_store(
        &mut self,
        frame: &mut Values<'_, '_>,
        pc: u32,
        operands: &[(u32, Value)],
        edit: MemberEdit,
        close: Option<crate::compile::expr::Close>,
    ) -> Result<bool, HeapFault> {
        let MemberEdit { handle, .. } = edit;
        // Keep the existing physical eligibility guard until its replacement
        // qualifies on every execution path in the atomic authority cutover.
        if self.arena.any_cold() || !self.resident_room(lash_kernel_state::arena::VALUE_BYTES) {
            return Ok(false);
        }
        let Some(statement) = frame.state.statement else {
            return Ok(false);
        };
        // Store packets follow their canonical Pay packets. Cell writes keep
        // their own close and submit that unpaid debt with the boundary.
        let charges = if matches!(edit.member, Member::Variable) {
            let Ok(charges) = room::charges::ChargeProgram::event_packet(
                self.exe,
                frame.state.callee.plan.code,
                statement,
                pc,
                close,
            ) else {
                return Ok(false);
            };
            Some(charges)
        } else {
            None
        };
        let (stream, statement, _) = self.exe.statement(statement);
        let Some(roles) = statement.source_roles_at(stream, pc) else {
            return Ok(false);
        };
        if roles.len() != operands.len()
            || roles
                .iter()
                .any(|(reg, _)| !operands.iter().any(|(input, _)| *input == reg.0))
        {
            return Ok(false);
        }
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let Some(unpaid) = self.source_next_statement(frame) else {
            return Ok(false);
        };
        let at = Point {
            site: statement.site.clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(false);
        };
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("member write has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("member source owners failed verification"))?;
        let mut inputs = Vec::new();
        let mut selectors = Vec::new();
        let mut values = Vec::new();
        let event = interval
            .events()
            .next()
            .ok_or_else(|| malformed("member event disappeared"))?;
        for selector in &event.inputs {
            let room::Owner::Operand(site) = selector else {
                return Ok(false);
            };
            let Some((reg, _)) = roles.iter().find(|(_, actual)| actual.as_ref() == site) else {
                return Ok(false);
            };
            let value = operands
                .iter()
                .find(|(input, _)| *input == reg.0)
                .map(|(_, value)| *value)
                .ok_or_else(|| malformed("verified member operand disappeared"))?;
            let owner = slots
                .root(selector, self.task, activation.invocation.id())
                .ok_or_else(|| malformed("member operand has no source owner"))?;
            inputs.push(owner);
            selectors.push(selector.clone());
            values.push(value);
        }
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        for input in &inputs {
            if ledger.holding(*input, self.heap.pages())?.is_none() {
                return Ok(false);
            }
        }
        // Ask the canonical member owner for both the graph change and the
        // post-member release. Aliases and self-edges are never summed here.
        let member = ledger
            .prepare_source_members_boundary(
                self.arena
                    .registration()
                    .map_err(|error| malformed(&error.to_string()))?,
                self.heap,
                std::slice::from_ref(&edit),
                &[],
                &[],
            )
            .map_err(super::unit::member_fault)?;
        let delta = i128::from(member.balance()?.total()?) - i128::from(ledger.balance()?.total()?);
        let released = member.release_cost(&inputs, self.heap.pages())?;
        drop(member);
        let mut measures = std::collections::BTreeMap::new();
        for (selector, value) in selectors.iter().zip(&values) {
            let held = lash_kernel_state::kernel_internals::logical::Holding::from_native(
                self.arena,
                self.heap.pages(),
                value,
            )?;
            measures.insert(Measure::Local(selector.clone()), i128::from(held.bytes()));
        }
        measures.insert(Measure::MemberDelta(at.site.clone()), delta);
        measures.insert(
            Measure::Release(
                interval
                    .events()
                    .next()
                    .ok_or_else(|| malformed("member event disappeared"))?
                    .inputs
                    .clone(),
            ),
            i128::from(released),
        );
        let facts = LeafExecution(
            [(
                at.clone(),
                LeafFacts {
                    dependencies: std::iter::once(handle)
                        .chain(values.iter().filter_map(Value::handle))
                        .collect(),
                    values: measures,
                    ..Default::default()
                },
            )]
            .into(),
        );
        let mut encoding = TokenWriter::new();
        encoding.seq(3);
        encoding.text("lash.source.member-store");
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
            at,
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close,
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        drop(ledger);
        let published = self.publish_source_boundary(
            frame.state.callee,
            activation,
            publication::Prefix {
                interval,
                facts,
                start,
                next,
                allowance,
            },
            crate::machine::source_publication::SourceMember {
                charges: charges.as_ref(),
                edit: edit.clone(),
                inputs,
                operands: values,
            },
            |arena, _, edits, write| {
                let [edit] = edits.as_slice() else {
                    unreachable!("a checked list publication changed member arity");
                };
                write.publish(arena, edit.clone());
            },
        )?;
        #[cfg(any(test, feature = "testing"))]
        if published {
            match edit.member {
                Member::Variable => crate::machine::islands::witness::cell_published(),
                _ => crate::machine::islands::witness::member_published(),
            }
        }
        Ok(published)
    }
}
