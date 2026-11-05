//! Statically selected views. Fixture implementations use their own values,
//! phase capabilities and graph packets, never production seals.
use lash_kernel_doc::{Measure, Value, ValueKind, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::{
    LeaseAllocation, LeaseFacts, LeaseFault, LeaseGraphExit, LeaseMeasure, LeaseMember, LeaseOp,
    LeaseStep, LeaseValue, ReadyLeaseStep, StateLease,
};
use std::sync::Arc;

pub(super) enum Selector<V> {
    List(u64),
    Table(V),
    Field(Arc<str>),
    Variable,
}
pub(super) enum Operation<'a, V> {
    List(&'a [V]),
    Map(&'a [(V, V)]),
    Set(&'a [V]),
    Record(&'a [(V, V)]),
    Tuple(&'a [V]),
    Cell(&'a V),
    Text(&'a str),
    Bytes(&'a [u8]),
    Member {
        object: V,
        member: Selector<V>,
        next: Option<V>,
    },
    Grow {
        object: V,
        additional: u64,
    },
    Hold {
        value: V,
        count: u64,
    },
    Release {
        value: V,
        count: u64,
    },
    HoldCapture {
        cell: V,
        count: u64,
    },
    ReleaseCapture {
        cell: V,
        count: u64,
    },
}
#[derive(Clone, Copy)]
pub(super) enum FactMeasure {
    Members,
    Payload,
    Released,
    Depth,
}
pub(super) trait Facts {
    fn delta(&self) -> i128;
    fn peak(&self) -> u64;
    fn measure(&self, selector: FactMeasure) -> Option<u64>;
}
pub(super) trait Ready {
    type Value: Copy;
    fn result(&self) -> Option<Self::Value>;
    fn accept(self) -> Option<Self::Value>;
}
pub(super) trait Step {
    type Facts: Facts;
    type Value: Copy;
    type Ready: Ready<Value = Self::Value>;
    fn facts(&self) -> &Self::Facts;
    fn reserve(self) -> Result<Self::Ready, HeapFault>;
}
pub(super) trait Graph {
    type Value: Copy;
    fn value(&self, value: Self::Value) -> Result<Value, HeapFault>;
}
pub(super) trait View {
    type Value: Copy;
    type Graph: Graph<Value = Self::Value>;
    type Step<'a>: Step<Value = Self::Value>
    where
        Self: 'a;
    fn import(&mut self, value: &Value) -> Result<Self::Value, HeapFault>;
    fn scalar(&self, value: Self::Value) -> Result<Value, HeapFault>;
    fn kind(&self, value: Self::Value) -> Result<ValueKind, HeapFault>;
    fn measure(&mut self, value: Self::Value, selector: Measure) -> Result<u64, LeaseFault>;
    fn read(
        &mut self,
        base: Self::Value,
        selector: &Selector<Self::Value>,
    ) -> Result<Self::Value, LeaseFault>;
    fn stage<'a>(
        &'a mut self,
        operation: Operation<'a, Self::Value>,
    ) -> Result<Self::Step<'a>, LeaseFault>;
    fn freeze_graph(&mut self) -> Result<Self::Graph, HeapFault>;
}
impl Facts for LeaseFacts {
    fn delta(&self) -> i128 {
        self.delta()
    }
    fn peak(&self) -> u64 {
        self.peak()
    }
    fn measure(&self, selector: FactMeasure) -> Option<u64> {
        self.measure(match selector {
            FactMeasure::Members => &LeaseMeasure::MEMBERS,
            FactMeasure::Payload => &LeaseMeasure::PAYLOAD,
            FactMeasure::Released => &LeaseMeasure::RELEASED,
            FactMeasure::Depth => &LeaseMeasure::DEPTH,
        })
    }
}
impl Ready for ReadyLeaseStep<'_, '_> {
    type Value = LeaseValue;
    fn result(&self) -> Option<LeaseValue> {
        self.result()
    }
    fn accept(self) -> Option<LeaseValue> {
        self.accept()
    }
}
impl<'a, 's> Step for LeaseStep<'a, 's> {
    type Facts = LeaseFacts;
    type Value = LeaseValue;
    type Ready = ReadyLeaseStep<'a, 's>;
    fn facts(&self) -> &LeaseFacts {
        self.facts()
    }
    fn reserve(self) -> Result<Self::Ready, HeapFault> {
        self.reserve()
    }
}
impl Graph for LeaseGraphExit {
    type Value = LeaseValue;
    fn value(&self, value: LeaseValue) -> Result<Value, HeapFault> {
        self.value(value)
    }
}
fn member(selector: &Selector<LeaseValue>) -> LeaseMember {
    match selector {
        Selector::List(index) => LeaseMember::List(*index),
        Selector::Table(key) => LeaseMember::Table(*key),
        Selector::Field(name) => LeaseMember::Field(Arc::clone(name)),
        Selector::Variable => LeaseMember::Variable,
    }
}
impl<'s> View for StateLease<'s> {
    type Value = LeaseValue;
    type Graph = LeaseGraphExit;
    type Step<'a>
        = LeaseStep<'a, 's>
    where
        Self: 'a;
    fn import(&mut self, value: &Value) -> Result<LeaseValue, HeapFault> {
        self.import(value)
    }
    fn scalar(&self, value: LeaseValue) -> Result<Value, HeapFault> {
        self.scalar(value)
    }
    fn kind(&self, value: LeaseValue) -> Result<ValueKind, HeapFault> {
        self.kind(value)
    }
    fn measure(&mut self, value: LeaseValue, selector: Measure) -> Result<u64, LeaseFault> {
        self.measure(value, selector)
    }
    fn read(
        &mut self,
        base: LeaseValue,
        selector: &Selector<LeaseValue>,
    ) -> Result<LeaseValue, LeaseFault> {
        let object = self.object(base)?;
        self.read(object, &member(selector))
    }
    fn stage<'a>(
        &'a mut self,
        operation: Operation<'a, LeaseValue>,
    ) -> Result<Self::Step<'a>, LeaseFault> {
        let op = match operation {
            Operation::List(values) => LeaseOp::Allocate(LeaseAllocation::list(values)),
            Operation::Map(entries) => LeaseOp::Allocate(LeaseAllocation::map(entries)),
            Operation::Set(values) => LeaseOp::Allocate(LeaseAllocation::set(values)),
            Operation::Record(entries) => LeaseOp::Allocate(LeaseAllocation::record(entries)),
            Operation::Tuple(values) => LeaseOp::Allocate(LeaseAllocation::tuple(values)),
            Operation::Cell(value) => LeaseOp::Allocate(LeaseAllocation::cell(value)),
            Operation::Text(value) => LeaseOp::Allocate(LeaseAllocation::text(value)),
            Operation::Bytes(value) => LeaseOp::Allocate(LeaseAllocation::bytes(value)),
            Operation::Member {
                object,
                member: selector,
                next,
            } => LeaseOp::Member {
                object: self.object(object)?,
                member: member(&selector),
                next,
            },
            Operation::Grow { object, additional } => LeaseOp::Grow {
                object: self.object(object)?,
                additional,
            },
            Operation::Hold { value, count } => LeaseOp::Hold { value, count },
            Operation::Release { value, count } => LeaseOp::Release { value, count },
            Operation::HoldCapture { cell, count } => LeaseOp::HoldCapture {
                cell: self.object(cell)?,
                count,
            },
            Operation::ReleaseCapture { cell, count } => LeaseOp::ReleaseCapture {
                cell: self.object(cell)?,
                count,
            },
        };
        self.stage(op)
    }
    fn freeze_graph(&mut self) -> Result<LeaseGraphExit, HeapFault> {
        self.materialize_graph()
    }
}
