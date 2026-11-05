use super::storage::{Graph, View};
use super::{RegionError, invalid};
use crate::compile::room::Refused;
use lash_kernel_doc::{Value, heap::HeapFault};

#[derive(Clone, Copy)]
pub(super) enum Operand<V: Copy> {
    Immediate(Value),
    Dynamic(V),
}
impl<V: Copy> Operand<V> {
    pub(super) fn scalar<S: View<Value = V>>(self, view: &S) -> Result<Value, RegionError> {
        match self {
            Self::Immediate(value) => {
                if !matches!(value, Value::Int(_) | Value::Bool(_)) {
                    return Err(invalid(Refused::Facts));
                }
                Ok(value)
            }
            Self::Dynamic(value) => view.scalar(value).map_err(RegionError::Heap),
        }
    }
    pub(super) fn bind<S: View<Value = V>>(self, view: &mut S) -> Result<V, HeapFault> {
        match self {
            Self::Immediate(value) => view.import(&value),
            Self::Dynamic(value) => Ok(value),
        }
    }
    pub(super) fn materialize<G: Graph<Value = V>>(self, graph: &G) -> Result<Value, HeapFault> {
        match self {
            Self::Immediate(value) => Ok(value),
            Self::Dynamic(value) => graph.value(value),
        }
    }
}
