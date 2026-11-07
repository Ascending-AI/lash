//! Detached literal storage for exact source reads.
use super::*;
use lash_kernel_state::kernel_internals::logical::Owner;

fn literal_recipe<'a>(
    arena: &'a Arena,
    literal: &doc::Literal,
    link: lash_kernel_value::content_ids::LinkId,
) -> (doc::Nursery<'a>, doc::Value) {
    use doc::{Literal, Value};
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
    (nursery, value)
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
    let (nursery, value) = literal_recipe(arena, literal, link);
    let installation = arena
        .prepare_install(nursery.into_new())
        .map_err(native_fault)?;
    super::construction::boundary::finish(installation, heap, &[(output, value)], usize::MAX, true)
}
