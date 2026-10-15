//! Descriptor-only logical holding facts shared by execution anchors.
use lash_kernel_doc::Value;

pub(crate) fn value_bytes(value: &Value) -> u64 {
    lash_kernel_doc::heap_values::local_cost(value)
}
pub(crate) fn within_depth(value: &Value, budget: usize) -> bool {
    let depth = match value {
        Value::Tuple(tuple) => tuple.summary.depth,
        Value::Error(error) => error.depth,
        _ => 0,
    };
    depth as usize <= budget
}
