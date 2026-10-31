//! Detached literal storage shared by precise and packet source reads.
use super::*;
use crate::compile::room::{Measure, Owner as Selector, Point};
use lash_kernel_state::{
    arena::PreparedBatch,
    kernel_internals::{
        logical::{Holding, Owner, PublicationValue},
        stored_pages,
    },
};

pub(in crate::machine) struct Literal<'literal, 'heap> {
    pub(in crate::machine) literal: &'literal doc::Literal,
    pub(in crate::machine) link: lash_kernel_value::content_ids::LinkId,
    pub(in crate::machine) output: Owner,
    pub(in crate::machine) at: Point,
    pub(in crate::machine) physical_room: u64,
    pub(in crate::machine) heap: &'heap PagedHeap,
}
pub(in crate::machine) struct PreparedLiteral {
    pub(in crate::machine) batch: PreparedBatch,
    pub(in crate::machine) value: doc::Value,
    pub(in crate::machine) holding: Holding,
    at: Point,
}
impl StoragePreparation for Literal<'_, '_> {
    type Prepared = PreparedLiteral;
    fn prepare<'a>(
        self,
        arena: &'a mut Arena,
        actual: Program,
    ) -> Result<Option<PreparedStorage<'a, Self::Prepared>>, HeapFault> {
        let [event] = actual.events() else {
            return Ok(None);
        };
        if !event.inputs.is_empty()
            || !event.outputs.is_empty()
            || !event.entities.is_empty()
            || event.frame.is_some()
            || event.member.is_some()
            || !event.scratch.is_empty()
        {
            return Ok(None);
        }
        use doc::{Literal, Value, heap::PageValue};
        // Page construction may fail. Do it before reserving any nursery slot.
        let page = match self.literal {
            Literal::Null => PageValue::Null,
            Literal::Absent => PageValue::Absent,
            Literal::Bool(value) => PageValue::Bool(*value),
            Literal::Float(value) => PageValue::Float(*value),
            Literal::Int(value) => {
                PageValue::Int(stored_pages::int(self.heap.pages(), value.clone())?)
            }
            Literal::Text(value) => PageValue::Text(stored_pages::text(self.heap.pages(), value)?),
            Literal::Bytes(value) => {
                PageValue::Bytes(stored_pages::bytes(self.heap.pages(), value.as_slice())?)
            }
            Literal::Function(function) => {
                PageValue::Function(Box::new(lash_kernel_value::names::CallableId {
                    link: self.link,
                    function: *function,
                }))
            }
        };
        let mut nursery = doc::Nursery::unaccounted(arena.objects());
        let value = match self.literal {
            Literal::Null => Value::Null,
            Literal::Absent => Value::Absent,
            Literal::Bool(value) => Value::Bool(*value),
            Literal::Float(value) => Value::Float(*value),
            Literal::Int(value) => nursery.integer(value.clone()),
            Literal::Text(value) => nursery.text(value),
            Literal::Bytes(value) => nursery.bytes(value.as_slice()),
            Literal::Function(function) => nursery.function(lash_kernel_value::names::CallableId {
                link: self.link,
                function: *function,
            }),
        };
        // Compatibility eligibility only until the complete logical cutover.
        // This physical estimate supplies no guest price or refusal.
        if arena.install_bound(nursery.built()) > self.physical_room {
            nursery.discard();
            return Ok(None);
        }
        let installation = arena
            .prepare_install(nursery.into_new())
            .map_err(native_fault)?;
        let (registration, batch) = installation.into_parts();
        let holding = Holding::from_page(self.heap.pages(), &page)?;
        let mut event = event.clone();
        event
            .outputs
            .push((self.output, PublicationValue::Page(page)));
        Ok(Some(PreparedStorage {
            registration,
            storage: PreparedLiteral {
                batch,
                value,
                holding,
                at: self.at,
            },
            actual: Program::new(vec![event]),
        }))
    }
    fn facts(prepared: &Self::Prepared, facts: &mut LeafExecution) -> Result<(), HeapFault> {
        let facts = facts
            .0
            .get_mut(&prepared.at)
            .ok_or_else(|| malformed("literal facts disappeared"))?;
        facts.values.insert(
            Measure::Local(Selector::Operand(prepared.at.site.clone())),
            i128::from(prepared.holding.bytes()),
        );
        Ok(())
    }
}
