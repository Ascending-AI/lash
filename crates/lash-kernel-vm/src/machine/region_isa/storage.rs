//! The lease view region values are imported into and read back from.
use lash_kernel_doc::{Value, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::{LeaseGraphExit, LeaseValue, StateLease};

pub(super) trait Graph {
    type Value: Copy;
    fn value(&self, value: Self::Value) -> Result<Value, HeapFault>;
}
pub(super) trait View {
    type Value: Copy;
    type Graph: Graph<Value = Self::Value>;
    fn import(&mut self, value: &Value) -> Result<Self::Value, HeapFault>;
    fn scalar(&self, value: Self::Value) -> Result<Value, HeapFault>;
}
impl Graph for LeaseGraphExit {
    type Value = LeaseValue;
    fn value(&self, value: LeaseValue) -> Result<Value, HeapFault> {
        self.value(value)
    }
}
impl View for StateLease<'_> {
    type Value = LeaseValue;
    type Graph = LeaseGraphExit;
    fn import(&mut self, value: &Value) -> Result<LeaseValue, HeapFault> {
        self.import(value)
    }
    fn scalar(&self, value: LeaseValue) -> Result<Value, HeapFault> {
        self.scalar(value)
    }
}
