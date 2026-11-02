//! A constructor's normalized detached body is its canonical publication recipe.
use crate::compile::room::{self, Measure, Point};
use crate::machine::source_publication::{
    LeafExecution, PreparedStorage, StoragePreparation, malformed,
};
use lash_kernel_doc::{self as doc, Value, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, PreparedBatch},
    kernel_internals::logical::{Ledger, Owner, Program, PublicationValue as PV, RootValue},
};
pub(in crate::machine) struct Construction<'a> {
    pub(in crate::machine) object: doc::NewObject,
    pub(in crate::machine) output: Owner,
    pub(in crate::machine) at: Point,
    pub(in crate::machine) release: Box<[room::Owner]>,
    pub(in crate::machine) ledger: std::rc::Rc<std::cell::RefCell<Ledger>>,
    pub(in crate::machine) depth_limit: usize,
    pub(in crate::machine) heap: &'a lash_kernel_state::kernel_internals::PagedHeap,
}
pub(in crate::machine) struct Prepared {
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) value: Value,
    at: Point,
    release: Box<[room::Owner]>,
    obligation: (Measure, u64),
    released: u64,
}
impl StoragePreparation for Construction<'_> {
    type Prepared = Prepared;
    const MATERIALIZES: bool = true;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.outputs.is_empty()
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
            || !event.scratch.is_empty()
        {
            return Ok(None);
        }
        let Some(prepared) = super::boundary::prepare_collection(
            arena,
            self.heap,
            self.object,
            self.output,
            self.depth_limit,
        )?
        else {
            return Ok(None);
        };
        let super::boundary::PreparedGraph {
            mut registration,
            batch,
            mut outputs,
            entities,
        } = prepared;
        let output = outputs
            .pop()
            .ok_or_else(|| malformed("construction lost its output"))?;
        let value = output.value;
        let page = output.page;
        let mut event = event.clone();
        let (obligation, released) = if let [entity] = entities.as_slice() {
            let members = entity.body.member_bytes(self.heap, registration.arena())?;
            let released = self.ledger.borrow().release_cost_preserving_body(
                registration.arena(),
                self.heap,
                &event.inputs,
                &entity.body,
            )?;
            ((Measure::Members(self.at.site.clone()), members), released)
        } else {
            let released = self.ledger.borrow().release_cost_preserving(
                registration.arena(),
                self.heap,
                &event.inputs,
                &[(self.output, RootValue::Page(&page))],
            )?;
            (
                (
                    Measure::Local(room::Owner::Operand(self.at.site.clone())),
                    output.holding.bytes(),
                ),
                released,
            )
        };
        event.entities = entities;
        event.outputs.push((self.output, PV::Page(page)));
        Ok(Some(PreparedStorage {
            registration,
            storage: Prepared {
                batch,
                value,
                at: self.at,
                release: self.release,
                obligation,
                released,
            },
            actual: Program::new(vec![event]),
        }))
    }
    fn facts(storage: &Prepared, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&storage.at)
            .ok_or_else(|| malformed("construction lost its source event"))?;
        facts.values.insert(
            storage.obligation.0.clone(),
            i128::from(storage.obligation.1),
        );
        facts.values.insert(
            Measure::Release(storage.release.clone()),
            i128::from(storage.released),
        );
        Ok(())
    }
}
