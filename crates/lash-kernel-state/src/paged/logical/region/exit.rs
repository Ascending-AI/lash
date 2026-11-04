//! Affine current-prefix packet. Only testing can construct the declaration.
#![allow(dead_code, reason = "declaration handoff for Amendment 2")]
use super::storage::{LeaseObject, LeaseValue};
use super::{StateLease, unavailable};
use crate::paged::logical::PublicationValue;
use lash_kernel_doc::{Value, arena::ArenaHandle, heap::HeapFault};

pub struct LeaseGraphExit {
    sealed: Seal,
}
enum Seal {
    #[cfg(any(test, feature = "perf-witness"))]
    Fixture,
}
impl LeaseGraphExit {
    pub fn value(&self, _value: LeaseValue) -> Result<Value, HeapFault> {
        Err(unavailable())
    }
    pub fn object(&self, _object: LeaseObject) -> Result<ArenaHandle, HeapFault> {
        Err(unavailable())
    }
    pub fn publication(&self, _value: LeaseValue) -> Result<PublicationValue, HeapFault> {
        Err(unavailable())
    }
}
impl StateLease<'_> {
    pub fn materialize_graph(&mut self) -> Result<LeaseGraphExit, HeapFault> {
        Err(unavailable())
    }
}
#[cfg(any(test, feature = "perf-witness"))]
pub(super) fn fixture() -> LeaseGraphExit {
    LeaseGraphExit {
        sealed: Seal::Fixture,
    }
}
