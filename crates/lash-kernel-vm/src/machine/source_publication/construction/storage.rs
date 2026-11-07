//! A constructor prepares one canonical graph and authenticated source boundary.
use crate::compile::room::{self, Measure, Point};
use crate::machine::source_publication::{
    BoundaryPreparation, ExternalEntry, PublicationError, malformed,
};
use lash_kernel_doc::{self as doc, Value, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, PreparedBatch},
    kernel_internals::logical::{
        Dependencies, Holding, Owner, PublicationValue as ExactValue, VirtualBody,
    },
};
pub(in crate::machine) enum ConstructionValue<'a> {
    Collection(doc::NewObject),
    Cell(Value),
    Literal {
        literal: &'a doc::Literal,
        link: lash_kernel_value::content_ids::LinkId,
    },
    Closure {
        identity: std::sync::Arc<doc::arena::ClosureIdentity>,
        captures: Vec<doc::arena::ArenaHandle>,
        capture_names: Vec<lash_kernel_value::Name>,
        code: doc::arena::LocalCode,
    },
}
pub(in crate::machine) struct Construction<'a> {
    pub(in crate::machine) charges: Option<&'a room::charges::ChargeProgram>,
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
        self.prepare_inner(arena, entry)
    }
}
impl Construction<'_> {
    fn prepare_inner<'a>(
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
        let mut graph = match &self.value {
            ConstructionValue::Collection(object) => {
                let Some(graph) = super::boundary::prepare_collection(
                    arena,
                    self.heap,
                    object.clone(),
                    self.output,
                    self.depth_limit,
                )?
                else {
                    return Ok(None);
                };
                graph
            }
            ConstructionValue::Cell(value) => {
                super::boundary::prepare_cell(arena, self.heap, *value, self.output)?
            }
            ConstructionValue::Literal { literal, link } => {
                super::boundary::prepare_literal(arena, self.heap, literal, *link, self.output)?
            }
            ConstructionValue::Closure {
                identity,
                captures,
                code,
                ..
            } => super::boundary::prepare_closure(
                arena,
                self.heap,
                identity.clone(),
                captures.clone(),
                *code,
                self.output,
            )?,
        };
        let [output] = graph.outputs.as_slice() else {
            return Err(malformed("construction lost its output"));
        };
        let released = ledger.release_cost_preserving_graph(
            graph.registration.arena(),
            heap,
            &self.inputs,
            &output.proof,
        )?;
        let obligation = if let [entity] = graph.proof.entities() {
            let members = graph
                .proof
                .body(0)
                .bytes()
                .checked_sub(doc::LogicalHeldV1::OBJECT)
                .ok_or_else(|| malformed("construction body lost its header"))?;
            match &entity.body {
                VirtualBody::Closure(captures) => (
                    Measure::CaptureEdges(self.at.site.clone()),
                    captures.len() as u64,
                ),
                _ => (Measure::Members(self.at.site.clone()), members),
            }
        } else {
            (
                Measure::Local(room::Owner::Operand(self.at.site.clone())),
                output.holding.bytes(),
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
        let roots = ExactRoots {
            inputs: self.inputs.clone(),
            outputs: vec![(self.output, ExactValue::Prepared(output.proof.clone()))],
            ..Default::default()
        };
        let external = ExternalFacts {
            operands: &self.operands,
            entities: graph.proof.entities(),
            ..Default::default()
        };
        let debt = self.charges.and_then(|charges| charges.bind(&[]));
        if self.charges.is_some() && debt.is_none() {
            return Ok(None);
        }
        let allowance = if let Some(debt) = &debt {
            let Some(allowance) = debt.native_allowance(prefix.allowance.safe_point) else {
                return Ok(None);
            };
            allowance
        } else {
            prefix.allowance
        };
        let construction = (
            match &self.value {
                ConstructionValue::Collection(object) => Some(object),
                _ => None,
            },
            match &self.value {
                ConstructionValue::Literal { literal, link } => Some((*literal, *link)),
                _ => None,
            },
            match &self.value {
                ConstructionValue::Closure {
                    identity,
                    captures,
                    capture_names,
                    ..
                } => Some((
                    identity.as_ref(),
                    captures.as_slice(),
                    capture_names.as_slice(),
                )),
                _ => None,
            },
            match &self.value {
                ConstructionValue::Cell(value) => Some(*value),
                _ => None,
            },
        );
        let bound = event.bind_construction(
            frontier,
            ledger,
            graph.registration.arena(),
            heap,
            &mut prefix.facts,
            roots,
            prefix.next,
            allowance,
            resources,
            external,
            construction,
            debt.as_ref(),
        );
        let Ok(bound) = bound else {
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
        let event = bound.finish_external(frontier, ledger, transaction);
        match event {
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
