//! Root atom ownership enters before the canonical native completion debt.
use super::*;
use crate::admission::Allowance;
use crate::machine::source_publication::{BoundaryPreparation, ExternalEntry};
use lash_kernel_state::kernel_internals::logical::{Holding, PublicationValue as PV};
use lash_kernel_value::tokens::{TokenCodec, TokenWriter, logical_digest};
use room::{
    ExitKind, Measure, Phase, Point,
    publication::{Occurrence, Position as SourcePosition},
};

struct RootArguments<'a> {
    values: &'a [Value],
    sessions: crate::machine::SourceSessionReads,
    outputs: Vec<Owner>,
    selectors: Vec<room::Owner>,
    placement: &'a room::placement::BindingPlacement,
    invocation: &'a Invocation,
    slots: &'a [SlotState],
    code: crate::compile::CodeId,
}
impl BoundaryPreparation for RootArguments<'_> {
    type Prepared = ();
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, ())>, HeapFault> {
        let ExternalEntry {
            event,
            frontier,
            ledger,
            heap,
            mut prefix,
            resources,
        } = entry;
        let Some(activation) =
            self.placement
                .capture(self.code, self.invocation, ledger, heap, self.slots)?
        else {
            return Ok(None);
        };
        let facts = prefix
            .facts
            .0
            .get_mut(&prefix.start.at)
            .ok_or_else(|| malformed("root atom facts disappeared"))?;
        for (selector, value) in self.selectors.into_iter().zip(self.values) {
            let holding = Holding::from_native(arena, heap.pages(), value)?;
            facts
                .values
                .insert(Measure::Local(selector), i128::from(holding.bytes()));
        }
        let bound_result = event.bind_root_arguments(
            frontier,
            ledger,
            arena,
            heap,
            &mut prefix.facts,
            room::publication::ExactRoots {
                activation: Some(activation),
                inputs: vec![],
                outputs: self
                    .outputs
                    .into_iter()
                    .zip(self.values.iter().copied().map(PV::Native))
                    .collect(),
            },
            prefix.next,
            prefix.allowance,
            resources,
            &self.sessions,
        );
        let Ok(bound) = bound_result else {
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
    pub(super) fn publish_root_arguments(
        &mut self,
        frame: &mut Values<'_, '_>,
        site: &OperatorSite,
        values: &[Value],
    ) -> Result<bool, HeapFault> {
        let Some(id) = site.root else {
            return Ok(true);
        };
        if frame.state.statement != Some(id)
            || frame
                .state
                .source
                .as_ref()
                .is_none_or(|a| a.original.is_some())
        {
            return Ok(false);
        }
        let (stream, statement, _) = self.exe.statement(id);
        let crate::compile::Form::Action(action) = &statement.form else {
            return Ok(false);
        };
        let crate::compile::ActionKind::Call { callee, args } = &action.kind else {
            return Ok(false);
        };
        if callee.library() != Some(site.lib)
            || args.len() != values.len()
            || site.pc != statement.ops.start
            || !matches!(stream.ops[site.pc as usize], lash_kernel_exec::Op::Act)
        {
            return Ok(false);
        }
        let source = &self.exe.code(frame.state.callee.plan.code).source_room;
        let at = Point {
            site: action.site.clone(),
            phase: Phase::Enter,
        };
        let Ok(interval) = source.compile_event(&at, ExitKind::Action) else {
            return Ok(false);
        };
        let Some(placement) = frame.state.callee.compiled.placement.as_deref() else {
            return Ok(false);
        };
        self.logical
            .borrow_mut()
            .transfer_source(self.arena, self.heap, &[], &[])?;
        let activation = frame
            .state
            .source
            .as_mut()
            .ok_or_else(|| malformed("root operator has no source activation"))?;
        let selectors: Vec<_> = (0..values.len())
            .map(|index| room::Owner::Atom {
                at: at.site.clone(),
                index: index as u32,
            })
            .collect();
        let slots = source
            .owner_slots()
            .map_err(|_| malformed("root atom owners failed verification"))?;
        let outputs = selectors
            .iter()
            .map(|selector| {
                slots
                    .root(selector, self.task, activation.invocation.id())
                    .ok_or_else(|| malformed("root atom has no canonical owner"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut encoding = TokenWriter::new();
        encoding.seq(2);
        encoding.text("lash.source.packet-root-atoms");
        at.site.encode(&mut encoding);
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
            unpaid: Some(Point {
                site: at.site.clone(),
                phase: Phase::Commit,
            }),
            ..start.clone()
        };
        if let Some(frontier) = activation.frontier.as_mut()
            && frontier.departs(&start)
        {
            frontier.handoff(&mut self.logical.borrow_mut(), start.clone())?;
        }
        let sessions = self.session.source_reads(args, &self.logical.borrow());
        let invocation = activation.invocation.clone();
        self.publish_source_boundary(
            frame.state.callee,
            activation,
            publication::Prefix {
                interval,
                facts: LeafExecution(
                    [(
                        at,
                        LeafFacts {
                            dependencies: values.iter().filter_map(Value::handle).collect(),
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
            RootArguments {
                values,
                sessions,
                outputs,
                selectors,
                placement,
                invocation: &invocation,
                slots: frame.slots,
                code: frame.state.callee.plan.code,
            },
            |_, _, edits, ()| {
                debug_assert!(edits.is_empty());
            },
        )
    }
}
