//! A scalar iteration keeps its verified source lifetimes across backedges.
//! Only entry binds ledger owners. Dynamic prices use the original indexed
//! source equations; the final transaction publishes actual surviving values.
use crate::compile::room::placement::DenseActivation;
use crate::compile::room::region::{ScalarFact, ScalarRegion, ScalarRegionValues};
use crate::compile::room::{
    self, CheckedRoom, CompiledInterval, Execution, Facts, Measure, Owner, Point, Refused, Source,
};
use lash_kernel_doc::{TaskId, Value, heap::HeapFault};
use lash_kernel_state::{
    arena::{Arena, Registration},
    kernel_internals::{
        PagedHeap,
        logical::{self, Invocation, Ledger, RootValue, Slot},
    },
};
use lash_kernel_value::{FunctionId, sites::Site};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

struct EventPrices {
    at: Point,
    measures: Box<[(Measure, Box<[usize]>)]>,
}

pub(crate) struct Recurrence {
    region: ScalarRegion,
    events: Box<[EventPrices]>,
    calls: Box<[(Point, room::CallSource)]>,
    survivors: Box<[(Slot, ScalarFact)]>,
    branch: Site,
    condition: ScalarFact,
}

/// The ledger borrow rules out normalization, publication and stale owners
/// throughout the kernel. Current values are source facts, not a frame mirror.
pub(crate) struct BoundRecurrence<'plan, 'ledger> {
    plan: &'plan Recurrence,
    ledger: &'ledger Ledger,
    invocation: Invocation,
    current: Vec<Value>,
    initial_bytes: u64,
    delta: i128,
    peak: u64,
    iterations: u64,
    identity: std::rc::Rc<()>,
}

pub(crate) struct Iteration {
    balance: room::Balance,
    before: i128,
    occurrence: u64,
    identity: std::rc::Rc<()>,
    survivors: Vec<Value>,
    peak: u64,
}

impl Recurrence {
    pub(crate) fn compile(
        source: &Arc<Source>,
        interval: Arc<CompiledInterval>,
        branch: Site,
    ) -> Result<Self, Refused> {
        let region = source.compile_scalar_region(interval)?;
        let slots = source.owner_slots()?;
        let root = |owner: &Owner| match slots.root(owner, TaskId::MAIN, 0) {
            Some(logical::Owner::Value { slot, .. }) => Some(slot),
            _ => None,
        };
        let mut live: BTreeMap<Slot, ScalarFact> = region.entries().collect();
        let mut events = Vec::new();
        let mut calls = Vec::new();
        let mut condition = None;
        for event in region.interval().events() {
            let result = region
                .results()
                .find(|(at, _)| **at == event.point)
                .map(|(_, fact)| fact);
            if event.point.phase == room::Phase::Test && event.point.site == branch {
                condition = event
                    .inputs
                    .first()
                    .and_then(root)
                    .and_then(|slot| live.get(&slot).copied());
            }
            let mut measures = Vec::new();
            for measure in event.peak.measures().chain(event.delta.measures()) {
                if measures.iter().any(|(prior, _)| prior == measure) {
                    continue;
                }
                let facts = match measure {
                    Measure::Local(owner) => vec![
                        if event.output.as_ref() == Some(owner) {
                            result.ok_or(Refused::Facts)?
                        } else {
                            *live
                                .get(&root(owner).ok_or(Refused::Source)?)
                                .ok_or(Refused::Retained)?
                        }
                        .index(),
                    ],
                    Measure::Release(owners) => {
                        let mut selected = BTreeSet::new();
                        for owner in owners.iter() {
                            if let Some(slot) = root(owner) {
                                selected.insert(slot);
                            } else {
                                for member in slots
                                    .group(owner, TaskId::MAIN, 0)
                                    .ok_or(Refused::Source)?
                                    .iter()
                                {
                                    let logical::Owner::Value { slot, .. } = member else {
                                        return Err(Refused::Source);
                                    };
                                    selected.insert(*slot);
                                }
                            }
                        }
                        selected
                            .into_iter()
                            .filter_map(|slot| live.get(&slot))
                            .map(|fact| fact.index())
                            .collect()
                    }
                    Measure::CallPeak {
                        callee: room::CallSource::Native(_),
                        ..
                    }
                    | Measure::CallDelta {
                        callee: room::CallSource::Native(_),
                        ..
                    } => continue,
                    _ => return Err(Refused::Source),
                };
                measures.push((measure.clone(), facts.into_boxed_slice()));
            }
            for measure in event.peak.measures() {
                if let Measure::CallPeak { callee, .. } = measure {
                    calls.push((event.point.clone(), *callee));
                }
            }
            events.push(EventPrices {
                at: event.point.clone(),
                measures: measures.into(),
            });
            live = region
                .recovery_after(&event.point)
                .ok_or(Refused::Exit)?
                .iter()
                .copied()
                .collect();
        }
        // A completed backedge recreates precisely the same source entry
        // bindings. Body-local bindings/operands have already left their scope.
        let entry: BTreeSet<_> = region.entries().map(|(slot, _)| slot).collect();
        if live.keys().copied().collect::<BTreeSet<_>>() != entry {
            return Err(Refused::Retained);
        }
        Ok(Self {
            region,
            events: events.into(),
            calls: calls.into(),
            survivors: live.into_iter().collect(),
            branch,
            condition: condition.ok_or(Refused::Facts)?,
        })
    }
    pub(crate) fn region(&self) -> &ScalarRegion {
        &self.region
    }
    pub(crate) fn calls(&self) -> impl Iterator<Item = (&Point, room::CallSource)> {
        self.calls.iter().map(|(at, callee)| (at, *callee))
    }
    pub(crate) fn entry_values(&self, activation: &DenseActivation<'_>) -> Option<Vec<Value>> {
        let mut values = vec![Value::Null; self.region.fact_count()];
        for (slot, fact) in self.region.entries() {
            values[fact.index()] = activation.value(slot).filter(|value| scalar(*value))?;
        }
        Some(values)
    }
    #[expect(
        clippy::too_many_arguments,
        reason = "one authenticated scalar loop entry"
    )]
    pub(crate) fn bind<'plan, 'ledger>(
        &'plan self,
        ledger: &'ledger Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        invocation: &Invocation,
        activation: &DenseActivation<'_>,
        values: &[Value],
        calls: &[CheckedRoom],
    ) -> Result<Option<BoundRecurrence<'plan, 'ledger>>, HeapFault> {
        let Some(bound) = self.region.bind_activation(
            ledger,
            arena,
            heap,
            invocation,
            activation,
            ScalarRegionValues { values, calls },
        )?
        else {
            return Ok(None);
        };
        let initial_bytes = ledger.balance()?.total()?;
        let mut current = vec![Value::Null; self.region.fact_count()];
        for (_, fact) in self.region.entries() {
            current[fact.index()] = values[fact.index()];
        }
        // Keep the independently compiled lifetime proof in the entry check.
        // Subsequent candidates use its fixed source fact/recovery topology.
        let measured = self.balance(arena, values, calls)?;
        if measured.peak_extra != bound.peak() {
            return Ok(None);
        }
        Ok(Some(BoundRecurrence {
            plan: self,
            ledger,
            invocation: invocation.clone(),
            current,
            initial_bytes,
            delta: 0,
            peak: 0,
            iterations: 0,
            identity: std::rc::Rc::new(()),
        }))
    }
    fn balance(
        &self,
        arena: &Arena,
        values: &[Value],
        calls: &[CheckedRoom],
    ) -> Result<room::Balance, HeapFault> {
        if !matches!(values.get(self.condition.index()), Some(Value::Bool(true)))
            || values.len() != self.region.fact_count()
            || calls.len() != self.calls.len()
            || values.iter().any(|value| !scalar(*value))
        {
            return Err(invalid("scalar loop facts do not match their source"));
        }
        let prices = values
            .iter()
            .map(|value| arena.logical_local(value).map(i128::from))
            .collect::<Result<Vec<_>, _>>()?;
        self.region
            .interval()
            .evaluate_interval(&mut Prices {
                plan: self,
                prices: &prices,
                calls,
            })
            .map(|room| room.balance())
            .map_err(|_| invalid("scalar loop source equations refused"))
    }
}

struct Prices<'a> {
    plan: &'a Recurrence,
    prices: &'a [i128],
    calls: &'a [CheckedRoom],
}
struct EventFacts<'a> {
    event: &'a EventPrices,
    plan: &'a Recurrence,
    prices: &'a [i128],
    calls: &'a [CheckedRoom],
}
impl Execution for Prices<'_> {
    fn event(&mut self, at: &Point) -> Option<Box<dyn Facts + '_>> {
        let event = self.plan.events.iter().find(|event| event.at == *at)?;
        Some(Box::new(EventFacts {
            event,
            plan: self.plan,
            prices: self.prices,
            calls: self.calls,
        }))
    }
    fn choose(&mut self, site: &Site) -> Option<bool> {
        (*site == self.plan.branch).then_some(true)
    }
}
impl Facts for EventFacts<'_> {
    fn measure(&self, measure: &Measure) -> Option<i128> {
        self.event
            .measures
            .iter()
            .find(|(m, _)| m == measure)?
            .1
            .iter()
            .try_fold(0i128, |sum, fact| sum.checked_add(*self.prices.get(*fact)?))
    }
    fn call(&self, at: &Site, callee: room::CallSource) -> Option<&CheckedRoom> {
        let index = self
            .plan
            .calls
            .iter()
            .position(|(point, expected)| point.site == *at && *expected == callee)?;
        self.calls.get(index)
    }
    fn raised(&self) -> bool {
        false
    }
    fn resolved(&self, _: &Site) -> Option<FunctionId> {
        None
    }
}

impl BoundRecurrence<'_, '_> {
    pub(crate) fn entries(&self, values: &mut [Value]) {
        for (_, fact) in self.plan.region.entries() {
            values[fact.index()] = self.current[fact.index()];
        }
    }
    pub(crate) fn evaluate(
        &self,
        arena: &Arena,
        values: &[Value],
        calls: &[CheckedRoom],
        memory_bound: u64,
    ) -> Result<Option<Iteration>, HeapFault> {
        if self.plan.region.entries().any(|(_, fact)| {
            values
                .get(fact.index())
                .is_none_or(|value| !value.same(&self.current[fact.index()]))
        }) {
            return Ok(None);
        }
        let balance = self.plan.balance(arena, values, calls)?;
        let Some(peak) = self
            .delta
            .checked_add(i128::from(balance.peak_extra))
            .and_then(|value| u64::try_from(value.max(0)).ok())
        else {
            return Ok(None);
        };
        if self
            .initial_bytes
            .checked_add(peak)
            .is_none_or(|total| total > memory_bound)
        {
            return Ok(None);
        }
        Ok(Some(Iteration {
            balance,
            before: self.delta,
            occurrence: self.iterations,
            identity: std::rc::Rc::clone(&self.identity),
            survivors: self
                .plan
                .survivors
                .iter()
                .map(|(_, fact)| values[fact.index()])
                .collect(),
            peak,
        }))
    }
    pub(crate) fn advance(&mut self, iteration: Iteration) -> Result<(), HeapFault> {
        if iteration.before != self.delta
            || iteration.occurrence != self.iterations
            || !std::rc::Rc::ptr_eq(&iteration.identity, &self.identity)
        {
            return Err(invalid("scalar loop iteration is stale"));
        }
        self.delta = self
            .delta
            .checked_add(iteration.balance.retained_delta)
            .ok_or_else(|| invalid("scalar loop balance overflow"))?;
        self.peak = self.peak.max(iteration.peak);
        for ((slot, _), value) in self.plan.survivors.iter().zip(iteration.survivors) {
            let entry = self
                .plan
                .region
                .entries()
                .find(|(entry, _)| entry == slot)
                .ok_or_else(|| invalid("scalar backedge lost an entry"))?
                .1;
            self.current[entry.index()] = value;
        }
        self.iterations = self
            .iterations
            .checked_add(1)
            .ok_or_else(|| invalid("scalar loop occurrence overflow"))?;
        Ok(())
    }
    pub(crate) fn iterations(&self) -> u64 {
        self.iterations
    }
    pub(crate) fn prepare<'a>(
        self,
        registration: Registration<'a>,
        heap: &PagedHeap,
    ) -> Result<Option<logical::BoundaryTransaction<'a>>, HeapFault> {
        if self.iterations == 0 {
            return Ok(None);
        }
        let inputs: Vec<_> = self
            .plan
            .region
            .entries()
            .map(|(slot, _)| self.invocation.owner(slot))
            .collect();
        let outputs: Vec<_> = self
            .plan
            .region
            .entries()
            .map(|(slot, fact)| {
                (
                    self.invocation.owner(slot),
                    RootValue::Value(&self.current[fact.index()]),
                )
            })
            .collect();
        let transaction =
            self.ledger
                .prepare_source_boundary(registration, heap, &inputs, &outputs)?;
        if i128::from(transaction.balance()?.total()?) - i128::from(self.initial_bytes)
            != self.delta
        {
            return Ok(None);
        }
        Ok(transaction.require_source_room(self.peak))
    }
}
fn scalar(value: Value) -> bool {
    matches!(
        value,
        Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
    )
}
fn invalid(reason: &str) -> HeapFault {
    HeapFault::Malformed(reason.into())
}
