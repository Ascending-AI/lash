//! Frozen lease storage interfaces. Execution refuses until lane D installs it.
#![allow(dead_code, reason = "declaration handoff for Amendment 2")]
use super::{StateLease, unavailable};
use lash_kernel_doc::{
    Measure, NativeError, Value, ValueKind, arena::ClosureIdentity, heap::HeapFault,
};
use std::{convert::Infallible, marker::PhantomData, sync::Arc};

#[derive(Clone, Copy, Debug)]
pub struct LeaseValue {
    sealed: Infallible,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct LeaseObject {
    sealed: Infallible,
}
pub enum LeaseMember {
    List(u64),
    Table(LeaseValue),
    Field(Arc<str>),
    Variable,
}
pub struct LeaseAllocation<'a> {
    body: Allocation<'a>,
}
enum Allocation<'a> {
    List(&'a [LeaseValue]),
    Map(&'a [(LeaseValue, LeaseValue)]),
    Set(&'a [LeaseValue]),
    Record(&'a [(LeaseValue, LeaseValue)]),
    Tuple(&'a [LeaseValue]),
    Cell(&'a LeaseValue),
    Text(&'a str),
    Bytes(&'a [u8]),
    Closure {
        identity: Arc<ClosureIdentity>,
        captures: &'a [LeaseValue],
    },
}
impl<'a> LeaseAllocation<'a> {
    pub fn list(values: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::List(values),
        }
    }
    pub fn map(entries: &'a [(LeaseValue, LeaseValue)]) -> Self {
        Self {
            body: Allocation::Map(entries),
        }
    }
    pub fn set(values: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::Set(values),
        }
    }
    pub fn record(entries: &'a [(LeaseValue, LeaseValue)]) -> Self {
        Self {
            body: Allocation::Record(entries),
        }
    }
    pub fn tuple(values: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::Tuple(values),
        }
    }
    pub fn cell(value: &'a LeaseValue) -> Self {
        Self {
            body: Allocation::Cell(value),
        }
    }
    pub fn text(value: &'a str) -> Self {
        Self {
            body: Allocation::Text(value),
        }
    }
    pub fn bytes(value: &'a [u8]) -> Self {
        Self {
            body: Allocation::Bytes(value),
        }
    }
    pub fn closure(identity: Arc<ClosureIdentity>, captures: &'a [LeaseValue]) -> Self {
        Self {
            body: Allocation::Closure { identity, captures },
        }
    }
}
pub enum LeaseOp<'a> {
    Allocate(LeaseAllocation<'a>),
    Member {
        object: LeaseObject,
        member: LeaseMember,
        next: Option<LeaseValue>,
    },
    Grow {
        object: LeaseObject,
        additional: u64,
    },
    Hold {
        value: LeaseValue,
        count: u64,
    },
    Release {
        value: LeaseValue,
        count: u64,
    },
    HoldCapture {
        cell: LeaseObject,
        count: u64,
    },
    ReleaseCapture {
        cell: LeaseObject,
        count: u64,
    },
}
#[derive(Debug, thiserror::Error)]
pub enum LeaseFault {
    #[error("operation requires a state boundary")]
    BoundaryRequired,
    #[error(transparent)]
    Heap(#[from] HeapFault),
    #[error(transparent)]
    Native(#[from] NativeError),
}
pub struct LeaseMeasure {
    selector: MeasureSelector,
}
enum MeasureSelector {
    Members,
    Payload,
    Released,
    Depth,
}
impl LeaseMeasure {
    pub const MEMBERS: Self = Self {
        selector: MeasureSelector::Members,
    };
    pub const PAYLOAD: Self = Self {
        selector: MeasureSelector::Payload,
    };
    pub const RELEASED: Self = Self {
        selector: MeasureSelector::Released,
    };
    pub const DEPTH: Self = Self {
        selector: MeasureSelector::Depth,
    };
}
pub struct LeaseFacts {
    sealed: Infallible,
}
impl LeaseFacts {
    pub fn delta(&self) -> i128 {
        match self.sealed {}
    }
    pub fn peak(&self) -> u64 {
        match self.sealed {}
    }
    pub fn measure(&self, _selector: &LeaseMeasure) -> Option<u64> {
        match self.sealed {}
    }
}
pub struct LeaseStep<'a, 's> {
    sealed: Infallible,
    lease: PhantomData<&'a mut StateLease<'s>>,
}
pub struct ReadyLeaseStep<'a, 's> {
    sealed: Infallible,
    lease: PhantomData<&'a mut StateLease<'s>>,
}
impl<'a, 's> LeaseStep<'a, 's> {
    pub fn facts(&self) -> &LeaseFacts {
        match self.sealed {}
    }
    pub fn reserve(self) -> Result<ReadyLeaseStep<'a, 's>, HeapFault> {
        Err(unavailable())
    }
}
impl ReadyLeaseStep<'_, '_> {
    pub fn result(&self) -> Option<LeaseValue> {
        match self.sealed {}
    }
    pub fn accept(self) -> Option<LeaseValue> {
        match self.sealed {}
    }
}
pub struct LeaseCallable<'a> {
    sealed: Infallible,
    view: PhantomData<&'a ()>,
}
impl LeaseCallable<'_> {
    pub fn identity(&self) -> &lash_kernel_value::names::CallableId {
        match self.sealed {}
    }
    pub fn closure(&self) -> Option<&ClosureIdentity> {
        match self.sealed {}
    }
    pub fn captures(&self) -> &[LeaseValue] {
        match self.sealed {}
    }
}
impl<'s> StateLease<'s> {
    pub fn import(&mut self, _value: &Value) -> Result<LeaseValue, HeapFault> {
        Err(unavailable())
    }
    pub fn scalar(&self, _value: LeaseValue) -> Result<Value, HeapFault> {
        Err(unavailable())
    }
    pub fn object(&self, _value: LeaseValue) -> Result<LeaseObject, HeapFault> {
        Err(unavailable())
    }
    pub fn read(
        &mut self,
        _object: LeaseObject,
        _member: &LeaseMember,
    ) -> Result<LeaseValue, LeaseFault> {
        Err(LeaseFault::BoundaryRequired)
    }
    pub fn stage<'a>(&'a mut self, _op: LeaseOp<'a>) -> Result<LeaseStep<'a, 's>, LeaseFault> {
        Err(LeaseFault::BoundaryRequired)
    }
    pub fn callable(&mut self, _value: LeaseValue) -> Result<LeaseCallable<'_>, LeaseFault> {
        Err(LeaseFault::BoundaryRequired)
    }
    pub fn kind(&self, _value: LeaseValue) -> Result<ValueKind, HeapFault> {
        Err(unavailable())
    }
    pub fn measure(&mut self, _value: LeaseValue, _selector: Measure) -> Result<u64, LeaseFault> {
        Err(LeaseFault::BoundaryRequired)
    }
    pub fn version(&self, _object: LeaseObject) -> Result<u64, HeapFault> {
        Err(unavailable())
    }
}
