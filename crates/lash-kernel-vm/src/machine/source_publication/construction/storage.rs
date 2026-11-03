//! A constructor prepares one canonical graph and authenticated source boundary.
use crate::compile::room::{self, Measure, Point};
use crate::machine::source_publication::{
    BoundaryPreparation, ExternalEntry, PublicationError, malformed,
};
use lash_kernel_doc::{self as doc, Value, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, PreparedBatch},
    kernel_internals::logical::{
        Dependencies, Holding, Owner, PublicationValue as ExactValue, RootValue, VirtualBody,
    },
};
pub(in crate::machine) enum ConstructionValue<'a> {
    Collection(doc::NewObject),
    Literal {
        literal: &'a doc::Literal,
        link: lash_kernel_value::content_ids::LinkId,
    },
    Closure {
        identity: std::sync::Arc<doc::arena::ClosureIdentity>,
        captures: Vec<doc::arena::ArenaHandle>,
        code: doc::arena::LocalCode,
    },
}
pub(in crate::machine) struct Construction<'a> {
    pub(in crate::machine) value: ConstructionValue<'a>,
    pub(in crate::machine) output: Owner,
    pub(in crate::machine) at: Point,
    pub(in crate::machine) release: Box<[room::Owner]>,
    pub(in crate::machine) inputs: Vec<Owner>,
    pub(in crate::machine) operands: Vec<Value>,
    pub(in crate::machine) depth_limit: usize,
    pub(in crate::machine) heap: &'a lash_kernel_state::kernel_internals::PagedHeap,
}
pub(in crate::machine) struct Prepared {
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) value: Value,
    pub(in crate::machine) holding: Holding,
}
impl BoundaryPreparation for Construction<'_> {
    type Prepared = Prepared;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        entry: ExternalEntry<'_>,
    ) -> Result<Option<(room::publication::PreparedEvent<'a>, Prepared)>, HeapFault> {
        use room::publication::{ExactRoots, ExternalFacts};
        let ExternalEntry {
            event,
            frontier,
            ledger,
            heap,
            mut prefix,
            resources,
        } = entry;
        let mut handles: Vec<_> = self.operands.iter().filter_map(Value::handle).collect();
        if let ConstructionValue::Closure { captures, .. } = &self.value {
            handles.extend(captures);
        }
        let Some(dependencies) = Dependencies::capture(ledger, arena, &handles) else {
            return Ok(None);
        };
        let mut graph = match self.value {
            ConstructionValue::Collection(object) => {
                let Some(graph) = super::boundary::prepare_collection(
                    arena,
                    self.heap,
                    object,
                    self.output,
                    self.depth_limit,
                )?
                else {
                    return Ok(None);
                };
                graph
            }
            ConstructionValue::Literal { literal, link } => {
                super::boundary::prepare_literal(arena, self.heap, literal, link, self.output)?
            }
            ConstructionValue::Closure {
                identity,
                captures,
                code,
            } => super::boundary::prepare_closure(
                arena,
                self.heap,
                identity,
                captures,
                code,
                self.output,
            )?,
        };
        let [output] = graph.outputs.as_slice() else {
            return Err(malformed("construction lost its output"));
        };
        let (obligation, released) = if let [entity] = graph.entities.as_slice() {
            let members = entity.body.member_bytes(heap, graph.registration.arena())?;
            let released = ledger.release_cost_preserving_body(
                graph.registration.arena(),
                heap,
                &self.inputs,
                &entity.body,
            )?;
            let obligation = match &entity.body {
                VirtualBody::Closure(captures) => (
                    Measure::CaptureEdges(self.at.site.clone()),
                    captures.len() as u64,
                ),
                _ => (Measure::Members(self.at.site.clone()), members),
            };
            (obligation, released)
        } else {
            let released = ledger.release_cost_preserving(
                graph.registration.arena(),
                heap,
                &self.inputs,
                &[(self.output, RootValue::Page(&output.page))],
            )?;
            (
                (
                    Measure::Local(room::Owner::Operand(self.at.site.clone())),
                    output.holding.bytes(),
                ),
                released,
            )
        };
        let facts = prefix
            .facts
            .0
            .get_mut(&self.at)
            .ok_or_else(|| malformed("construction lost its source event"))?;
        facts.values.insert(obligation.0, i128::from(obligation.1));
        facts
            .values
            .insert(Measure::Release(self.release), i128::from(released));
        let Ok(bound) = event.bind_paid_external(
            frontier,
            ledger,
            graph.registration.arena(),
            heap,
            &mut prefix.facts,
            ExactRoots {
                inputs: self.inputs.clone(),
                outputs: vec![(self.output, ExactValue::Page(output.page.clone()))],
                ..Default::default()
            },
            prefix.next,
            prefix.allowance,
            resources,
            ExternalFacts {
                operands: &self.operands,
                entities: &graph.entities,
                ..Default::default()
            },
        ) else {
            return Ok(None);
        };
        let (transaction, mut storage) =
            graph.into_boundary(ledger, heap, &self.inputs, dependencies)?;
        let output = storage
            .outputs
            .pop()
            .ok_or_else(|| malformed("construction output disappeared"))?;
        // Each mutable body is summarized by the canonical construction owner.
        // Its source obligation was authenticated before consuming registration.
        debug_assert!(storage.members.len() <= 1);
        match bound.finish_external(frontier, ledger, transaction) {
            Ok(prepared) => Ok(Some((
                prepared,
                Prepared {
                    batch: storage.batch,
                    value: output.value,
                    holding: output.holding,
                },
            ))),
            Err(PublicationError::Miss(_)) => Ok(None),
            Err(PublicationError::Heap(error)) => Err(error),
        }
    }
}
