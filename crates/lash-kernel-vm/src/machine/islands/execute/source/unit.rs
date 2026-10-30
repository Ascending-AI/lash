//! The empty-tuple comparison keeps both source operands through the native.
use super::*;
use crate::admission::Allowance;
use lash_kernel_state::kernel_internals::logical::{
    Holding, JournalEvent, Program, PublicationValue as PV,
};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase as SourcePhase, Point, Selection,
    publication::{Occurrence, Position as SourcePosition},
};

impl<const OBSERVING: bool> Island<'_, OBSERVING> {
    pub(in crate::machine::islands::execute) fn publish_unit_comparison(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &UnitSite,
        value: Value,
        tuple: Value,
        units: u64,
    ) -> Result<bool, HeapFault> {
        let Some(selected) = &site.source else {
            return Ok(false);
        };
        self.source_bindings(frame)?;
        self.logical.flush(self.arena, self.heap)?;
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("unit comparison has no source activation"))?;
        let owners = source
            .owner_slots()
            .map_err(|_| malformed("comparison source owners failed verification"))?;
        let selector = |at: &lash_kernel_value::sites::Site| room::Owner::Operand(at.clone());
        let owner = |at: &lash_kernel_value::sites::Site| {
            owners
                .root(&selector(at), self.task, activation.invocation.id())
                .ok_or_else(|| malformed("comparison operand has no source owner"))
        };
        let left = owner(&selected.value)?;
        let right = owner(&selected.tuple)?;
        let output = owner(&selected.comparison)?;
        let at = |site: &lash_kernel_value::sites::Site| Point {
            site: site.clone(),
            phase: SourcePhase::Commit,
        };
        let left_at = at(&selected.value);
        let right_at = at(&selected.tuple);
        let comparison_at = at(&selected.comparison);
        let test = Point {
            site: selected.test.clone(),
            phase: SourcePhase::Test,
        };
        let route =
            [left_at.clone(), right_at.clone(), comparison_at.clone()].map(|at| Selection::Event {
                at,
                exit: ExitKind::Normal,
            });
        let store = self.logical.ledger();
        let ledger = store.borrow();
        if ledger.holding(right, self.heap.pages())?.is_some()
            || ledger.holding(output, self.heap.pages())?.is_some()
        {
            return Ok(false);
        }
        let held_left = ledger.holding(left, self.heap.pages())?.is_some();
        let from = if held_left { &right_at } else { &left_at };
        let Ok(interval) = source.compile_between(from, &comparison_at, &route, route.len()) else {
            return Ok(false);
        };
        let function = self.exe.lib(site.lib);
        let LibRun::Native { native, .. } = &function.run else {
            return Ok(false);
        };
        let semantics = self.arena.semantics();
        let values = [value, tuple];
        let contract = match native.resource_contract(doc::Args::new(
            &values,
            self.arena.objects(),
            &semantics,
        )) {
            Ok(Some(contract)) => contract,
            Err(doc::NativeError::Malformed(reason)) => return Err(malformed(reason)),
            Ok(None) | Err(_) => return Ok(false),
        };
        let callee = function.specialization.resource;
        if !matches!(callee, room::CallSource::Native(_)) {
            return Ok(false);
        }
        let Some(called) = room::CheckedRoom::native(function.specialization.function, &contract)
        else {
            return Ok(false);
        };
        let make_inputs = || {
            let mut events = Vec::new();
            if !held_left {
                events.push(JournalEvent {
                    outputs: vec![(left, PV::Native(value))],
                    ..Default::default()
                });
            }
            events.push(JournalEvent {
                outputs: vec![(right, PV::Native(tuple))],
                ..Default::default()
            });
            events
        };
        // The ledger's unpublished prefix owns the graph-aware release fact.
        // Neither a slot count nor a sum of private prices can replace it.
        let prefix = Program::new(make_inputs());
        let Some(evaluated_prefix) = prefix.evaluate(&ledger, self.arena) else {
            return Ok(false);
        };
        let prepared_prefix = evaluated_prefix
            .prepare(
                &ledger,
                self.arena
                    .registration()
                    .map_err(|error| malformed(&error.to_string()))?,
                self.heap,
                Program::new(make_inputs()),
            )
            .map_err(member_fault)?;
        let released = prepared_prefix.release_cost(&[left, right], self.heap.pages())?;
        drop(prepared_prefix);
        let left_bytes = Holding::from_native(self.arena, self.heap.pages(), &value)?.bytes();
        let tuple_bytes = Holding::from_native(self.arena, self.heap.pages(), &tuple)?.bytes();
        let mut dependencies = values.iter().filter_map(Value::handle).collect::<Vec<_>>();
        if let Some(binding) = selected.binding
            && let Some(SlotState::Cell(cell)) = frame.slots.get(binding as usize)
        {
            dependencies.push(*cell);
        }
        let facts = LeafExecution(
            [
                (
                    left_at.clone(),
                    LeafFacts {
                        dependencies: dependencies.clone(),
                        values: [(Measure::Local(selector(&selected.value)), left_bytes.into())]
                            .into(),
                        ..Default::default()
                    },
                ),
                (
                    right_at.clone(),
                    LeafFacts {
                        dependencies: dependencies.clone(),
                        values: [
                            (
                                Measure::Local(selector(&selected.tuple)),
                                tuple_bytes.into(),
                            ),
                            (
                                Measure::Release(Box::default()),
                                i128::from(ledger.release_cost(&[], self.heap.pages())?),
                            ),
                        ]
                        .into(),
                        ..Default::default()
                    },
                ),
                (
                    comparison_at.clone(),
                    LeafFacts {
                        dependencies,
                        values: [(
                            Measure::Release(
                                vec![selector(&selected.value), selector(&selected.tuple)].into(),
                            ),
                            released.into(),
                        )]
                        .into(),
                        call: Some((selected.comparison.clone(), callee, called)),
                    },
                ),
            ]
            .into(),
        );
        let make = || {
            let mut events = make_inputs();
            events.push(JournalEvent {
                inputs: vec![left, right],
                outputs: vec![(output, PV::Native(Value::Bool(false)))],
                ..Default::default()
            });
            Program::new(events)
        };
        let mut recovery = TokenWriter::new();
        recovery.seq(3);
        recovery.text("lash.source.unit-comparison");
        recovery.seq(4);
        for point in [&left_at, &right_at, &comparison_at, &test] {
            recovery.seq(2);
            point.site.encode(&mut recovery);
            recovery.unsigned(point.phase as u64);
        }
        recovery.seq(3);
        for owner in [left, right, output] {
            owner.encode(&mut recovery);
        }
        let position = SourcePosition {
            at: from.clone(),
            unpaid: Some(from.clone()),
            recovery: logical_digest(&recovery.into_bytes()),
            occurrence: Occurrence {
                task: self.task,
                invocation: activation.invocation.id(),
                loops: frame
                    .state
                    .control
                    .iter()
                    .filter_map(|control| match control {
                        Position::Loop { id, started, .. } => Some((
                            self.exe.stmt(*id).site.clone(),
                            if *id == selected.statement {
                                *started
                            } else {
                                started.saturating_sub(1)
                            },
                        )),
                        _ => None,
                    })
                    .collect(),
                action: None,
                departure: None,
            },
        };
        let next = SourcePosition {
            at: comparison_at,
            unpaid: Some(test),
            ..position.clone()
        };
        let allowance = Allowance {
            occurrences: vec![],
            before: 0,
            units: if self.accounting { units } else { 0 },
            close: if self.accounting { site.close } else { None },
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
                start: position,
                next,
                allowance,
            },
            make,
            |_, edits| debug_assert!(edits.is_empty()),
        )?;
        if published {
            witnessed!(SourceUnit, Commit);
        }
        Ok(published)
    }
}
pub(super) fn member_fault(
    error: lash_kernel_state::kernel_internals::logical::MemberFault,
) -> HeapFault {
    match error {
        lash_kernel_state::kernel_internals::logical::MemberFault::Heap(error) => error,
        lash_kernel_state::kernel_internals::logical::MemberFault::Native(error) => {
            publication::native_fault(error)
        }
    }
}
