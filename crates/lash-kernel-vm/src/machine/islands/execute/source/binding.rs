//! A local binding transfers the evaluated RHS occurrence at its source Commit.
use super::*;
use crate::admission::Allowance;
use crate::machine::islands::compiled::WriteSite;
use lash_kernel_state::kernel_internals::logical::PublicationValue as PV;
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

/// An exact local move retains the evaluated RHS provenance. Its source
/// charge close and owner transfer share the same affine acceptance.
struct SourceTransfer<'a> {
    charges: &'a room::charges::ChargeProgram,
    inputs: Vec<Owner>,
    input: Owner,
    output: Owner,
}
impl crate::machine::source_publication::BoundaryPreparation for SourceTransfer<'_> {
    type Prepared = ();
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: crate::machine::source_publication::ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, ())>, HeapFault> {
        let crate::machine::source_publication::ExternalEntry {
            event,
            frontier,
            ledger,
            heap,
            mut prefix,
            resources,
        } = entry;
        let Some(debt) = self.charges.bind(&[]) else {
            return Ok(None);
        };
        let Some(allowance) = debt.native_allowance(prefix.allowance.safe_point) else {
            return Ok(None);
        };
        let Ok(bound) = event.bind_external(
            frontier,
            ledger,
            arena,
            heap,
            &mut prefix.facts,
            room::publication::ExactRoots {
                inputs: self.inputs,
                outputs: vec![(self.output, PV::Existing(self.input))],
                ..Default::default()
            },
            prefix.next,
            allowance,
            resources,
            room::publication::ExternalFacts::default(),
            &debt,
        ) else {
            return Ok(None);
        };
        let registration = arena.registration().map_err(publication::native_fault)?;
        match bound.prepare(frontier, ledger, registration, heap) {
            Ok(prepared) => Ok(Some((prepared, ()))),
            Err(room::publication::PublicationError::Miss(_)) => Ok(None),
            Err(room::publication::PublicationError::Heap(error)) => Err(error),
        }
    }
}

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_binding(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &WriteSite,
        value: Value,
    ) -> Result<Option<bool>, HeapFault> {
        if site.accumulates
            || self.tape.is_some()
            || self.projection().is_some()
            || (!site.bind && matches!(frame.slots[site.at], SlotState::Empty))
        {
            return Ok(None);
        }
        if site.shared && !site.bind {
            return self.publish_cell_store(frame, site, value);
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
        let code = self.exe.code(frame.state.callee.plan.code);
        let source = &code.source_room;
        if site.shared && (source.persistent.contains_key(at) || !self.cell_room(1)) {
            return Ok(None);
        }
        let at = Point {
            site: (**at).clone(),
            phase: SourcePhase::Commit,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Normal) else {
            return Ok(None);
        };
        let Some(roles) = statement.source_roles_at(stream, site.pc) else {
            return Ok(None);
        };
        let Some((_, rhs)) = roles.iter().find(|(reg, _)| reg.0 == site.reg) else {
            return Ok(None);
        };
        let Some(unpaid) = self.source_next_statement(frame) else {
            return Ok(None);
        };
        let activation = frame
            .state
            .source
            .as_ref()
            .ok_or_else(|| malformed("binding has no source activation"))?;
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("binding source owners failed verification"))?;
        let selector = room::Owner::Operand((**rhs).clone());
        let input = slots
            .root(&selector, self.task, activation.invocation.id())
            .ok_or_else(|| malformed("binding RHS has no source owner"))?;
        let output = source
            .binding_root(
                code.slots_by_at[site.at],
                self.task,
                activation.invocation.id(),
            )
            .ok_or_else(|| malformed("binding has no canonical slot"))?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("binding has no source activation"))?;
        self.logical.flush(self.arena, self.heap)?;
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(input, self.heap.pages())?.is_none() {
            return Ok(None);
        }
        if site.bind && ledger.holding(output, self.heap.pages())?.is_some() {
            return Ok(Some(false));
        }
        let mut inputs = vec![input];
        let mut measures = std::collections::BTreeMap::new();
        if !site.bind {
            // The RHS already keeps every object retained by the replacement.
            // Removing the old binding therefore gives the exact source release.
            let released = ledger.release_cost(&[output], self.heap.pages())?;
            inputs.push(output);
            let event = interval
                .events()
                .next()
                .ok_or_else(|| malformed("binding event disappeared"))?;
            for measure in event.delta.measures() {
                if matches!(measure, Measure::Release(_)) {
                    measures.insert(measure.clone(), i128::from(released));
                }
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
        encoding.text("lash.source.local-binding");
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
            },
        };
        let next = SourcePosition {
            at: at.clone(),
            unpaid: Some(unpaid),
            ..start.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: 0,
            close: self.accounting.then_some(site.close).flatten(),
            strict: false,
            depth: 0,
            safe_point: self.charged,
        };
        drop(ledger);
        let prefix = publication::Prefix {
            interval,
            facts,
            start,
            next,
            allowance,
        };
        let Some(charges) = site.charges.as_deref() else {
            return Ok(None);
        };
        let published = if site.shared {
            self.publish_source_boundary(
                frame.state.callee,
                activation,
                prefix,
                publication::CellConstruction {
                    construction: crate::machine::source_publication::construction::storage::Construction {
                        charges: Some(charges),
                        value: crate::machine::source_publication::construction::storage::ConstructionValue::Cell(value),
                        output,
                        at: at.clone(),
                        release: vec![selector].into_boxed_slice(),
                        inputs,
                        operands: vec![value],
                        depth_limit: self.limits.value_depth(),
                        heap: self.heap,
                    },
                },
                |arena, _, edits, storage| {
                    debug_assert!(edits.is_empty());
                    storage.batch.publish(arena);
                    frame.slots[site.at] = SlotState::Cell(storage.handle);
                },
            )?
        } else {
            self.publish_source_boundary(
                frame.state.callee,
                activation,
                prefix,
                SourceTransfer {
                    charges,
                    inputs,
                    input,
                    output,
                },
                |_, _, edits, ()| {
                    debug_assert!(edits.is_empty());
                    frame.slots[site.at] = SlotState::Value(value);
                },
            )?
        };
        #[cfg(any(test, feature = "testing"))]
        if published && site.shared {
            crate::machine::islands::witness::cell_constructed();
        }
        Ok(Some(published))
    }
}
