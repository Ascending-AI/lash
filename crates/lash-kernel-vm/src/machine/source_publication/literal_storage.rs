//! Detached literal storage shared by precise and packet source reads.
use super::*;
use lash_kernel_state::kernel_internals::{
    logical::{Holding, Owner},
    stored_pages,
};

fn literal_recipe<'a>(
    arena: &'a Arena,
    heap: &PagedHeap,
    literal: &doc::Literal,
    link: lash_kernel_value::content_ids::LinkId,
) -> Result<(doc::heap::PageValue, doc::Nursery<'a>, doc::Value), HeapFault> {
    use doc::{Literal, Value, heap::PageValue};
    // Page construction may fail. Do it before reserving any nursery slot.
    let page = match literal {
        Literal::Null => PageValue::Null,
        Literal::Absent => PageValue::Absent,
        Literal::Bool(value) => PageValue::Bool(*value),
        Literal::Float(value) => PageValue::Float(*value),
        Literal::Int(value) => PageValue::Int(stored_pages::int(heap.pages(), value.clone())?),
        Literal::Text(value) => PageValue::Text(stored_pages::text(heap.pages(), value)?),
        Literal::Bytes(value) => {
            PageValue::Bytes(stored_pages::bytes(heap.pages(), value.as_slice())?)
        }
        Literal::Function(function) => {
            PageValue::Function(Box::new(lash_kernel_value::names::CallableId {
                link,
                function: *function,
            }))
        }
    };
    let mut nursery = doc::Nursery::unaccounted(arena.objects());
    let value = match literal {
        Literal::Null => Value::Null,
        Literal::Absent => Value::Absent,
        Literal::Bool(value) => Value::Bool(*value),
        Literal::Float(value) => Value::Float(*value),
        Literal::Int(value) => nursery.integer(value.clone()),
        Literal::Text(value) => nursery.text(value),
        Literal::Bytes(value) => nursery.bytes(value.as_slice()),
        Literal::Function(function) => nursery.function(lash_kernel_value::names::CallableId {
            link,
            function: *function,
        }),
    };
    Ok((page, nursery, value))
}

/// Literal storage for a compiled boundary has no physical eligibility price.
/// The caller supplies its source owner and authenticates the exact source exit.
pub(in crate::machine) fn prepare_literal<'a>(
    arena: &'a mut Arena,
    heap: &PagedHeap,
    literal: &doc::Literal,
    link: lash_kernel_value::content_ids::LinkId,
    output: Owner,
) -> Result<super::construction::boundary::PreparedGraph<'a>, HeapFault> {
    let (page, nursery, value) = literal_recipe(arena, heap, literal, link)?;
    let installation = arena
        .prepare_install(nursery.into_new())
        .map_err(native_fault)?;
    let (registration, batch) = installation.into_parts();
    let holding = Holding::from_page(heap.pages(), &page)?;
    Ok(super::construction::boundary::PreparedGraph {
        registration,
        batch,
        entities: Vec::new(),
        outputs: vec![super::construction::boundary::Output {
            owner: output,
            value,
            page,
            holding,
        }],
    })
}
