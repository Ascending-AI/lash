//! Separate fixture values and phase capabilities. No production seal is minted.
use super::fixture_phases::FixtureStep;
use super::storage::{self, Graph, View};
use super::*;
use lash_kernel_doc::{Measure, ValueKind, heap::HeapFault};
use lash_kernel_state::kernel_internals::logical::LeaseFault;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug)]
pub(super) enum FixtureValue {
    Scalar(Value),
    Object {
        lease: u64,
        index: u32,
        generation: u64,
    },
}
pub(super) struct Object {
    pub(super) identity: Value,
    pub(super) members: Vec<FixtureValue>,
    pub(super) fields: Box<[Arc<str>]>,
    pub(super) generation: u64,
    pub(super) version: u64,
    pub(super) retained: u64,
    pub(super) alive: bool,
}
pub(super) struct FixtureView {
    pub(super) lease: u64,
    pub(super) objects: Vec<Object>,
    pub(super) frozen: bool,
    pub(super) free: Vec<usize>,
    pub(super) references: Vec<(usize, u64)>,
    pub(super) work: Vec<(FixtureValue, i128)>,
    pub(super) next_identity: u64,
    pub(super) fail_reserve: bool,
    pub(super) fail_freeze: bool,
    pub(super) boundary_reads: bool,
}
pub(super) struct FixtureGraph {
    pub(super) next_identity: u64,
    pub(super) lease: u64,
    values: Box<[Option<(Value, u64)>]>,
    pub(super) bodies: Box<[Option<Snapshot>]>,
}
pub(super) fn bad(message: &str) -> HeapFault {
    HeapFault::Malformed(message.into())
}
impl FixtureView {
    pub(super) fn bind(programs: &programs::Programs, code: &Code) -> Result<Self, RegionError> {
        let _ = programs;
        static LEASE: AtomicU64 = AtomicU64::new(1);
        let lease = LEASE.fetch_add(1, Ordering::Relaxed);
        let mut objects = Vec::new();
        objects
            .try_reserve_exact(code.dependencies.len())
            .map_err(|_| invalid(Refused::Scratch))?;
        for dependency in &code.dependencies {
            objects.push(Object {
                identity: dependency.identity,
                fields: dependency.fields.clone(),
                members: dependency
                    .members
                    .iter()
                    .map(|value| FixtureValue::Scalar(*value))
                    .collect(),
                generation: 1,
                version: 0,
                retained: code
                    .initial
                    .iter()
                    .filter(|(reg, value)| {
                        matches!(reg, super::code::Register::Value(_))
                            && value.same(&dependency.identity)
                    })
                    .count()
                    .max(1) as u64,
                alive: true,
            });
        }
        Ok(Self {
            lease,
            objects,
            frozen: false,
            free: Vec::new(),
            references: Vec::new(),
            work: Vec::new(),
            next_identity: 1,
            fail_reserve: false,
            fail_freeze: false,
            boundary_reads: false,
        })
    }
    pub(super) fn object(&self, value: FixtureValue) -> Result<&Object, HeapFault> {
        let FixtureValue::Object {
            lease,
            index,
            generation,
        } = value
        else {
            return Err(bad("fixture object required"));
        };
        let object = self
            .objects
            .get(index as usize)
            .ok_or_else(|| bad("fixture object absent"))?;
        if lease != self.lease || generation != object.generation || !object.alive {
            return Err(bad("foreign or released fixture handle"));
        }
        Ok(object)
    }
}
impl View for FixtureView {
    type Value = FixtureValue;
    type Graph = FixtureGraph;
    type Step<'a> = FixtureStep<'a>;
    fn import(&mut self, value: &Value) -> Result<FixtureValue, HeapFault> {
        if self.frozen {
            return Err(bad("fixture graph is frozen"));
        }
        if let Some((index, object)) = self
            .objects
            .iter()
            .enumerate()
            .find(|(_, object)| object.alive && object.identity.same(value))
        {
            return Ok(FixtureValue::Object {
                lease: self.lease,
                index: index as u32,
                generation: object.generation,
            });
        }
        Ok(FixtureValue::Scalar(*value))
    }
    fn scalar(&self, value: FixtureValue) -> Result<Value, HeapFault> {
        match value {
            FixtureValue::Scalar(value) => Ok(value),
            FixtureValue::Object { .. } => Err(bad("fixture object is not scalar")),
        }
    }
    fn kind(&self, value: FixtureValue) -> Result<ValueKind, HeapFault> {
        match value {
            FixtureValue::Scalar(value) => Ok(match value {
                Value::Int(_) => ValueKind::Int,
                Value::Bool(_) => ValueKind::Bool,
                Value::Null => ValueKind::Null,
                Value::Absent => ValueKind::Absent,
                _ => return Err(bad("fixture scalar kind outside this law")),
            }),
            value => {
                self.object(value)?;
                Ok(if self.object(value)?.fields.is_empty() {
                    ValueKind::List
                } else {
                    ValueKind::Record
                })
            }
        }
    }
    fn measure(&mut self, value: FixtureValue, _selector: Measure) -> Result<u64, LeaseFault> {
        Ok(self.object(value)?.members.len() as u64)
    }
    fn read(
        &mut self,
        value: FixtureValue,
        selector: &storage::Selector<FixtureValue>,
    ) -> Result<FixtureValue, LeaseFault> {
        if self.frozen {
            return Err(bad("fixture graph is frozen").into());
        }
        if self.boundary_reads {
            return Err(LeaseFault::BoundaryRequired);
        }
        let object = self.object(value)?;
        let index = match selector {
            storage::Selector::List(index) => *index as usize,
            storage::Selector::Variable => 0,
            storage::Selector::Field(name) => object
                .fields
                .iter()
                .position(|field| field == name)
                .ok_or_else(|| bad("fixture field absent"))?,
            _ => return Err(bad("fixture selector is not indexed").into()),
        };
        object
            .members
            .get(index)
            .copied()
            .ok_or_else(|| bad("fixture member absent").into())
    }
    fn stage<'a>(
        &'a mut self,
        operation: storage::Operation<'a, FixtureValue>,
    ) -> Result<FixtureStep<'a>, LeaseFault> {
        self.stage_phase(operation)
    }
    fn freeze_graph(&mut self) -> Result<FixtureGraph, HeapFault> {
        if self.frozen || self.fail_freeze {
            return Err(bad("fixture graph freeze refused"));
        }
        self.frozen = true;
        let values = self
            .objects
            .iter()
            .map(|object| object.alive.then_some((object.identity, object.generation)))
            .collect();
        let mut graph = FixtureGraph {
            next_identity: self.next_identity,
            lease: self.lease,
            values,
            bodies: Box::new([]),
        };
        graph.bodies = self
            .objects
            .iter()
            .map(|object| {
                if !object.alive {
                    return Ok(None);
                }
                let members = object
                    .members
                    .iter()
                    .map(|value| graph.value(*value))
                    .collect::<Result<Box<[_]>, _>>()?;
                Ok(Some(Snapshot {
                    identity: object.identity,
                    fields: object.fields.clone(),
                    members,
                    version: object.version,
                    retained: object.retained,
                }))
            })
            .collect::<Result<_, HeapFault>>()?;
        Ok(graph)
    }
}
impl Graph for FixtureGraph {
    type Value = FixtureValue;
    fn value(&self, value: FixtureValue) -> Result<Value, HeapFault> {
        match value {
            FixtureValue::Scalar(value) => Ok(value),
            FixtureValue::Object {
                lease,
                index,
                generation,
            } => {
                let (value, expected) = self
                    .values
                    .get(index as usize)
                    .and_then(|value| value.as_ref())
                    .ok_or_else(|| bad("unbound or released fixture identity"))?;
                if lease != self.lease || generation != *expected {
                    return Err(bad("foreign fixture graph value"));
                }
                Ok(*value)
            }
        }
    }
}
pub(super) struct Snapshot {
    pub(super) identity: Value,
    pub(super) members: Box<[Value]>,
    pub(super) fields: Box<[Arc<str>]>,
    pub(super) version: u64,
    pub(super) retained: u64,
}
