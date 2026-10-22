//! Immutable results of decoding under an owner allocation budget.
use super::decode::GraphDecoder;
use super::{DecodeLimits, GraphRefusal, NodeId, NodeKind, SharedBytes, TokenCodec};
use lash_protocol_types::allocation::{AllocationBudget, AllocationHold};
use std::sync::Arc;

/// Read-only typed source with admission on the allocation readers retain.
pub struct AdmittedValue<T>(Arc<ValueSource<T>>);
struct ValueSource<T> {
    value: T,
    _allocation: AllocationHold,
}
impl<T> Clone for AdmittedValue<T> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}
impl<T: std::fmt::Debug> std::fmt::Debug for AdmittedValue<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.value.fmt(f)
    }
}
impl<T> std::ops::Deref for AdmittedValue<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0.value
    }
}

/// Source-admitted decoder. Typed and byte results retain their own holds;
/// dropping the decoder cannot uncharge a reader that escaped from it.
pub struct AdmittedGraphDecoder<'s> {
    pub(super) inner: GraphDecoder<'s>,
}
impl<'s> AdmittedGraphDecoder<'s> {
    fn take_allocation(&mut self) -> Result<AllocationHold, GraphRefusal> {
        let fresh = super::admission::DecodeAdmission::new_in(self.inner.admission.budget())?;
        Ok(std::mem::replace(&mut self.inner.admission, fresh).finish())
    }
    pub fn allocation_budget(&self) -> &AllocationBudget {
        self.inner.admission.budget()
    }
    pub fn limits(&self) -> DecodeLimits {
        self.inner.limits()
    }
    pub fn read(&self) -> (u64, u64) {
        self.inner.read()
    }
    pub fn child_kind(
        &mut self,
        parent: NodeId,
        child: NodeId,
        allowed: &[NodeKind],
    ) -> Result<NodeKind, GraphRefusal> {
        self.inner.child_kind(parent, child, allowed)
    }
    pub fn decode<T: TokenCodec>(
        &mut self,
        id: NodeId,
        expected: NodeKind,
    ) -> Result<AdmittedValue<T>, GraphRefusal> {
        let value = self.inner.decode(id, expected)?;
        let allocation = self.take_allocation()?;
        Ok(AdmittedValue(Arc::new(ValueSource {
            value,
            _allocation: allocation,
        })))
    }
    pub fn logical_body(
        &mut self,
        id: NodeId,
        expected: NodeKind,
    ) -> Result<SharedBytes, GraphRefusal> {
        let bytes = self.inner.logical_body(id, expected)?;
        let budget = self.allocation_budget().clone();
        let allocation = self.take_allocation()?;
        Ok(SharedBytes::from_admitted(bytes, allocation, budget))
    }
    pub fn bytes(&mut self, id: NodeId) -> Result<SharedBytes, GraphRefusal> {
        let bytes = self.inner.bytes(id)?;
        let budget = self.allocation_budget().clone();
        let allocation = self.take_allocation()?;
        Ok(SharedBytes::from_admitted(bytes, allocation, budget))
    }
    pub fn text(&mut self, id: NodeId) -> Result<AdmittedValue<String>, GraphRefusal> {
        let value = self.inner.text(id)?;
        let allocation = self.take_allocation()?;
        Ok(AdmittedValue(Arc::new(ValueSource {
            value,
            _allocation: allocation,
        })))
    }
}
