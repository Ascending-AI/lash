//! Scalar lifetime compilation. Interior owners occur only in the proof.
//! This is the ownership component of a region certificate. Charge, recovery
//! and composed-call authentication must join it before a runtime consumer
//! can elide source operations under shared acceptance.
use super::*;
use lash_kernel_doc::{TaskId, Value};
use lash_kernel_state::{
    arena::Arena,
    kernel_internals::{
        PagedHeap,
        logical::{self, Holding, Ledger, RootValue, Slot},
    },
};

#[derive(Clone, Default, Debug, PartialEq, Eq)]
struct Polynomial {
    fixed: i128,
    terms: BTreeMap<usize, i128>,
}
impl Polynomial {
    fn variable(index: usize) -> Self {
        Self {
            fixed: 0,
            terms: [(index, 1)].into(),
        }
    }
    fn add(&mut self, other: &Self, sign: i128) -> Result<(), Refused> {
        self.fixed = self
            .fixed
            .checked_add(other.fixed.checked_mul(sign).ok_or(Refused::Facts)?)
            .ok_or(Refused::Facts)?;
        for (&index, &coefficient) in &other.terms {
            let next = self
                .terms
                .get(&index)
                .copied()
                .unwrap_or(0)
                .checked_add(coefficient.checked_mul(sign).ok_or(Refused::Facts)?)
                .ok_or(Refused::Facts)?;
            if next == 0 {
                self.terms.remove(&index);
            } else {
                self.terms.insert(index, next);
            }
        }
        Ok(())
    }
    fn evaluate(&self, facts: &[i128]) -> Option<i128> {
        self.terms
            .iter()
            .try_fold(self.fixed, |sum, (&index, &coefficient)| {
                sum.checked_add(facts.get(index)?.checked_mul(coefficient)?)
            })
    }
    fn dominates(&self, other: &Self) -> bool {
        let mut difference = self.clone();
        difference.add(other, -1).is_ok()
            && difference.fixed >= 0
            && difference
                .terms
                .values()
                .all(|coefficient| *coefficient >= 0)
    }
}

/// A value fact is an entry owner or an evaluated source result. Its index is
/// independent of physical register placement and of runtime owner IDs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScalarFact(usize);
impl ScalarFact {
    pub(crate) fn index(self) -> usize {
        self.0
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Live {
    fact: ScalarFact,
    price: usize,
}
struct Entry {
    slot: Slot,
    live: Live,
}
struct ResultFact {
    at: Point,
    live: Live,
}
struct CallFact {
    at: Site,
    callee: CallSource,
    result: Live,
    price: usize,
}

/// The checked source owner compiles the topology once. The hot product has
/// reduced peak expressions and surviving transfers, not an ownership ISA.
pub(crate) struct ScalarRegion {
    source: Arc<Source>,
    interval: Arc<CompiledInterval>,
    entries: Box<[Entry]>,
    vacant: Box<[Slot]>,
    results: Box<[ResultFact]>,
    calls: Box<[CallFact]>,
    survivors: Box<[(Slot, Live)]>,
    peaks: Box<[Polynomial]>,
    delta: Polynomial,
    facts: usize,
    prices: usize,
}

struct Compiler<'a> {
    source: &'a Arc<Source>,
    slots: Arc<OwnerSlots>,
    live: BTreeMap<Slot, Live>,
    entries: Vec<Entry>,
    vacant: std::collections::BTreeSet<Slot>,
    results: Vec<ResultFact>,
    calls: Vec<CallFact>,
    peaks: Vec<Polynomial>,
    delta: Polynomial,
    facts: usize,
    prices: usize,
}
impl Compiler<'_> {
    fn slot(&self, owner: &Owner) -> Result<Slot, Refused> {
        match self.slots.root(owner, TaskId::MAIN, 0) {
            Some(logical::Owner::Value { slot, .. }) => Ok(slot),
            _ => Err(Refused::Source),
        }
    }
    fn fresh(&mut self) -> Live {
        let live = Live {
            fact: ScalarFact(self.facts),
            price: self.prices,
        };
        self.facts += 1;
        self.prices += 1;
        live
    }
    fn entry(&mut self, slot: Slot) -> Result<Live, Refused> {
        if let Some(live) = self.live.get(&slot) {
            return Ok(*live);
        }
        if self.vacant.contains(&slot) || self.entries.iter().any(|entry| entry.slot == slot) {
            return Err(Refused::Retained);
        }
        let live = self.fresh();
        self.entries.push(Entry { slot, live });
        self.live.insert(slot, live);
        Ok(live)
    }
    fn release(&mut self, slot: Slot) -> Result<Live, Refused> {
        let live = self.entry(slot)?;
        self.live.remove(&slot);
        self.delta.add(&Polynomial::variable(live.price), -1)?;
        Ok(live)
    }
    fn result(&mut self, at: &Point, owner: &Owner) -> Result<Live, Refused> {
        let slot = self.slot(owner)?;
        if self.live.contains_key(&slot) {
            return Err(Refused::Retained);
        }
        // An interior result must start vacant, including on repeated entry.
        if !self.entries.iter().any(|entry| entry.slot == slot) {
            self.vacant.insert(slot);
        }
        let live = self.fresh();
        self.results.push(ResultFact {
            at: at.clone(),
            live,
        });
        self.live.insert(slot, live);
        Ok(live)
    }
    fn peak(&mut self, extra: &Polynomial) -> Result<(), Refused> {
        let mut peak = self.delta.clone();
        peak.add(extra, 1)?;
        if !self.peaks.contains(&peak) {
            self.peaks.push(peak);
        }
        Ok(())
    }
    fn event(&mut self, event: &Event) -> Result<(), Refused> {
        let interval = self.source.compile_event(&event.point, ExitKind::Normal)?;
        if interval.acquisition(self.source).is_some() {
            let result =
                self.result(&event.point, event.output.as_ref().ok_or(Refused::Source)?)?;
            let price = Polynomial::variable(result.price);
            self.peak(&price)?;
            self.delta.add(&price, 1)?;
            return Ok(());
        }
        if interval.native_commit(self.source) {
            let result =
                self.result(&event.point, event.output.as_ref().ok_or(Refused::Source)?)?;
            let extra = match event.peak.measures().next() {
                Some(Measure::CallPeak {
                    at,
                    callee: callee @ CallSource::Native(_),
                }) => {
                    let price = self.prices;
                    self.prices += 1;
                    self.calls.push(CallFact {
                        at: at.clone(),
                        callee: *callee,
                        result,
                        price,
                    });
                    Polynomial::variable(price)
                }
                Some(Measure::Local(_)) => Polynomial::variable(result.price),
                _ => return Err(Refused::Source),
            };
            self.peak(&extra)?;
            for input in &event.inputs {
                self.release(self.slot(input)?)?;
            }
            self.delta.add(&Polynomial::variable(result.price), 1)?;
            return Ok(());
        }
        if let [Owner::Operand(rhs)] = &*event.inputs
            && let Some(output) = interval.binding_move(self.source, rhs)
        {
            let output = self.slot(output)?;
            let input = self.slot(&event.inputs[0])?;
            let live = self.release(input)?;
            // Assignment releases its previous holding. A declaration requires
            // vacancy and never manufactures an entry fact for its new binding.
            if event
                .delta
                .measures()
                .any(|measure| matches!(measure, Measure::Release(_)))
            {
                self.release(output)?;
            } else {
                if self.live.contains_key(&output) {
                    return Err(Refused::Retained);
                }
                self.vacant.insert(output);
            }
            self.live.insert(output, live);
            self.delta.add(&Polynomial::variable(live.price), 1)?;
            return Ok(());
        }
        if event.point.phase == Phase::Commit
            && !self.source.assigns_cell(&event.point.site)
            && let [Owner::Operand(_)] = &*event.inputs
        {
            let mut node = doc::Node::Block(&self.source.body);
            for index in &event.point.site.path[self.source.site.path.len()..] {
                node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
            }
            if let doc::Node::Stmt(doc::Stmt::Assign {
                place: doc::Place::Variable(name),
                ..
            }) = node
            {
                let output = Owner::Visible {
                    at: event.point.site.clone(),
                    name: name.clone(),
                };
                let delta =
                    Amount::default().term(Measure::Release(vec![output.clone()].into()), -1);
                if event.output.is_some()
                    || !event.entities.is_empty()
                    || event.peak != Amount::default()
                    || event.delta != delta
                {
                    return Err(Refused::Source);
                }
                let output = self.slot(&output)?;
                let input = self.slot(&event.inputs[0])?;
                let live = self.release(input)?;
                self.release(output)?;
                self.live.insert(output, live);
                self.delta.add(&Polynomial::variable(live.price), 1)?;
                return Ok(());
            }
        }
        // A source condition consumes its evaluated test without a new holding.
        if event.point.phase == Phase::Test
            && event.output.is_none()
            && event.peak == Amount::default()
            && event.entities.is_empty()
            && matches!(event.delta.terms.as_slice(), [(Measure::Release(owners), -1)] if **owners == *event.inputs)
        {
            for input in &event.inputs {
                self.release(self.slot(input)?)?;
            }
            return Ok(());
        }
        if interval.scope_release(self.source, &event.point.site) {
            // Group membership is derived from the immutable owner table. All
            // values in an entered scalar scope are known to this proof.
            let groups = [
                Owner::Scope(event.point.site.clone()),
                Owner::Operands(event.point.site.clone()),
            ];
            for group in &groups {
                let members = self
                    .slots
                    .group(group, TaskId::MAIN, 0)
                    .ok_or(Refused::Source)?;
                for member in members.iter() {
                    let logical::Owner::Value { slot, .. } = member else {
                        return Err(Refused::Source);
                    };
                    if self.live.contains_key(slot) {
                        self.release(*slot)?;
                    } else if !self.entries.iter().any(|entry| entry.slot == *slot) {
                        self.vacant.insert(*slot);
                    }
                }
            }
            return Ok(());
        }
        // Nested block entry has no owner or charge effect. Its scope lifetime
        // is authenticated by the interval's already checked control route.
        if event.point.phase == Phase::Enter
            && event.point.site != self.source.site
            && event.inputs.is_empty()
            && event.output.is_none()
            && event.entities.is_empty()
            && event.peak == Amount::default()
            && event.delta == Amount::default()
        {
            return Ok(());
        }
        Err(Refused::Source)
    }
}

impl Source {
    pub(crate) fn compile_scalar_region(
        self: &Arc<Self>,
        interval: Arc<CompiledInterval>,
    ) -> Result<ScalarRegion, Refused> {
        self.verify()?;
        if interval.owner() != self.owner
            || interval.defining_source() != &self.site
            || interval.exit() != ExitKind::Normal
        {
            return Err(Refused::Source);
        }
        let mut compiler = Compiler {
            source: self,
            slots: self.owner_slots()?,
            live: BTreeMap::new(),
            entries: Vec::new(),
            vacant: Default::default(),
            results: Vec::new(),
            calls: Vec::new(),
            peaks: Vec::new(),
            delta: Polynomial::default(),
            facts: 0,
            prices: 0,
        };
        for event in interval.events() {
            compiler.event(event)?;
        }
        compiler.peak(&Polynomial::default())?;
        let region = ScalarRegion {
            source: Arc::clone(self),
            interval,
            entries: compiler.entries.into(),
            vacant: compiler.vacant.into_iter().collect(),
            results: compiler.results.into(),
            calls: compiler.calls.into(),
            survivors: compiler.live.into_iter().collect(),
            peaks: compiler.peaks.into(),
            delta: compiler.delta,
            facts: compiler.facts,
            prices: compiler.prices,
        };
        region.verify()?;
        Ok(region)
    }
}

/// Values are canonical scalar answers, indexed by the compiled source fact
/// map. Calls are checked native contracts, never supplied peak byte counts.
pub(crate) struct ScalarRegionValues<'a> {
    pub(crate) values: &'a [Value],
    pub(crate) calls: &'a [CheckedRoom],
}
pub(crate) struct BoundScalarRegion<'a> {
    region: &'a ScalarRegion,
    stamp: logical::Stamp,
    invocation: logical::Invocation,
    values: Vec<Value>,
    peak: u64,
    delta: i128,
}
impl ScalarRegion {
    #[cfg(test)]
    pub(crate) fn understate_peak_for_law(&mut self) {
        self.peaks = Box::new([]);
    }
    /// The verifier evaluates the original source Amount expressions over
    /// symbolic holdings. It does not use the emitter's acquisition/move
    /// classifications or trust its canceled lifetimes and peak expressions.
    pub(crate) fn verify(&self) -> Result<(), Refused> {
        let slots = self.source.owner_slots()?;
        let root = |owner: &Owner| match slots.root(owner, TaskId::MAIN, 0) {
            Some(logical::Owner::Value { slot, .. }) => Some(slot),
            _ => None,
        };
        let mut live: BTreeMap<Slot, Live> = self
            .entries
            .iter()
            .map(|entry| (entry.slot, entry.live))
            .collect();
        let mut delta = Polynomial::default();
        let mut result_index = 0;
        for event in self.interval.events() {
            let result = self
                .results
                .get(result_index)
                .filter(|result| result.at == event.point);
            if result.is_some() {
                result_index += 1;
            }
            let members = |owner: &Owner| -> Result<Vec<Slot>, Refused> {
                if let Some(slot) = root(owner) {
                    return Ok(vec![slot]);
                }
                slots
                    .group(owner, TaskId::MAIN, 0)
                    .ok_or(Refused::Source)?
                    .iter()
                    .map(|owner| match owner {
                        logical::Owner::Value { slot, .. } => Ok(*slot),
                        _ => Err(Refused::Source),
                    })
                    .collect()
            };
            let amount = |amount: &Amount| -> Result<Polynomial, Refused> {
                let mut expression = Polynomial {
                    fixed: amount.fixed,
                    terms: BTreeMap::new(),
                };
                for (measure, coefficient) in &amount.terms {
                    let term = match measure {
                        Measure::Local(owner) if event.output.as_ref() == Some(owner) => {
                            Polynomial::variable(result.ok_or(Refused::Facts)?.live.price)
                        }
                        Measure::Release(owners) => {
                            let mut released = Polynomial::default();
                            let mut seen = std::collections::BTreeSet::new();
                            for owner in owners {
                                for slot in members(owner)? {
                                    if seen.insert(slot)
                                        && let Some(held) = live.get(&slot)
                                    {
                                        released.add(&Polynomial::variable(held.price), 1)?;
                                    }
                                }
                            }
                            released
                        }
                        Measure::CallPeak { at, callee } => {
                            let call = self
                                .calls
                                .iter()
                                .find(|call| call.at == *at && call.callee == *callee)
                                .ok_or(Refused::Source)?;
                            Polynomial::variable(call.price)
                        }
                        Measure::CallDelta { at, callee } => {
                            let call = self
                                .calls
                                .iter()
                                .find(|call| call.at == *at && call.callee == *callee)
                                .ok_or(Refused::Source)?;
                            Polynomial::variable(call.result.price)
                        }
                        _ => return Err(Refused::Source),
                    };
                    expression.add(&term, *coefficient)?;
                }
                Ok(expression)
            };
            let mut peak = delta.clone();
            peak.add(&amount(&event.peak)?, 1)?;
            if !self.peaks.iter().any(|bound| bound.dominates(&peak)) {
                return Err(Refused::Peak);
            }
            delta.add(&amount(&event.delta)?, 1)?;
            let input = event
                .inputs
                .first()
                .and_then(root)
                .and_then(|slot| live.get(&slot).copied());
            let mut released = std::collections::BTreeSet::new();
            for measure in event.delta.measures() {
                if let Measure::Release(owners) = measure {
                    for owner in owners {
                        released.extend(members(owner)?);
                    }
                }
            }
            for owner in &event.inputs {
                let slot = root(owner).ok_or(Refused::Source)?;
                if !live.contains_key(&slot) {
                    return Err(Refused::Retained);
                }
                released.insert(slot);
            }
            for slot in released {
                live.remove(&slot);
            }
            if let Some(output) = &event.output {
                live.insert(
                    root(output).ok_or(Refused::Source)?,
                    result
                        .map(|result| result.live)
                        .or(input)
                        .ok_or(Refused::Retained)?,
                );
            } else {
                let mut node = doc::Node::Block(&self.source.body);
                for index in &event.point.site.path[self.source.site.path.len()..] {
                    node = *node.children().get(*index as usize).ok_or(Refused::Path)?;
                }
                if let doc::Node::Stmt(doc::Stmt::Assign {
                    place: doc::Place::Variable(name),
                    ..
                }) = node
                {
                    let output = root(&Owner::Visible {
                        at: event.point.site.clone(),
                        name: name.clone(),
                    })
                    .ok_or(Refused::Source)?;
                    live.insert(output, input.ok_or(Refused::Retained)?);
                }
            }
        }
        if result_index != self.results.len()
            || delta != self.delta
            || live.into_iter().ne(self.survivors.iter().copied())
        {
            return Err(Refused::Retained);
        }
        if !self.peaks.iter().any(|peak| peak.dominates(&delta)) {
            return Err(Refused::Peak);
        }
        Ok(())
    }
    pub(crate) fn entries(&self) -> impl Iterator<Item = (Slot, ScalarFact)> + '_ {
        self.entries
            .iter()
            .map(|entry| (entry.slot, entry.live.fact))
    }
    pub(crate) fn results(&self) -> impl Iterator<Item = (&Point, ScalarFact)> + '_ {
        self.results
            .iter()
            .map(|result| (&result.at, result.live.fact))
    }
    pub(crate) fn fact_count(&self) -> usize {
        self.facts
    }
    pub(crate) fn bind<'a>(
        &'a self,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
        invocation: &logical::Invocation,
        values: ScalarRegionValues<'_>,
    ) -> Result<Option<BoundScalarRegion<'a>>, doc::heap::HeapFault> {
        let logical::Owner::Frame { task, .. } = invocation.frame() else {
            return Ok(None);
        };
        if ledger
            .invocation(task, invocation.id(), heap.pages())
            .as_ref()
            .ok()
            != Some(invocation)
        {
            return Ok(None);
        }
        if values.values.len() != self.facts
            || values.calls.len() != self.calls.len()
            || values.values.iter().any(|value| {
                !matches!(
                    value,
                    Value::Int(_) | Value::Float(_) | Value::Bool(_) | Value::Null | Value::Absent
                )
            })
        {
            return Ok(None);
        }
        let mut prices = vec![0_i128; self.prices];
        let price = |value: &Value, arena: &mut Arena| -> Result<u64, doc::heap::HeapFault> {
            Ok(Holding::from_native(arena, heap.pages(), value)?.bytes())
        };
        for entry in &self.entries {
            let value = &values.values[entry.live.fact.0];
            let Some(holding) = ledger.holding(invocation.owner(entry.slot), heap.pages())? else {
                return Ok(None);
            };
            let mut scalar = true;
            holding.visit_edges(heap.pages(), &mut |_, _| {
                scalar = false;
                Ok(())
            })?;
            if !scalar || holding.bytes() != price(value, arena)? {
                return Ok(None);
            }
            prices[entry.live.price] = i128::from(holding.bytes());
        }
        for slot in &self.vacant {
            if ledger
                .holding(invocation.owner(*slot), heap.pages())?
                .is_some()
            {
                return Ok(None);
            }
        }
        for result in &self.results {
            prices[result.live.price] =
                i128::from(price(&values.values[result.live.fact.0], arena)?);
        }
        for (call, room) in self.calls.iter().zip(values.calls) {
            let Some(balance) = room.for_call(call.callee, None) else {
                return Ok(None);
            };
            if balance.retained_delta != prices[call.result.price] {
                return Ok(None);
            }
            prices[call.price] = i128::from(balance.peak_extra);
        }
        let Some(peak) = self
            .peaks
            .iter()
            .try_fold(0_i128, |peak, expr| Some(peak.max(expr.evaluate(&prices)?)))
            .and_then(|peak| u64::try_from(peak).ok())
        else {
            return Ok(None);
        };
        let Some(delta) = self.delta.evaluate(&prices) else {
            return Ok(None);
        };
        Ok(Some(BoundScalarRegion {
            region: self,
            stamp: ledger.stamp(),
            invocation: invocation.clone(),
            values: values.values.to_vec(),
            peak,
            delta,
        }))
    }
}
impl BoundScalarRegion<'_> {
    pub(crate) fn peak(&self) -> u64 {
        self.peak
    }
    /// Only live-out roots are staged. No constituent event creates a runtime
    /// journal entry. The caller must carry the shared resource acceptance and
    /// exact source frontier through this preparation and its atomic commit.
    pub(crate) fn prepare(
        self,
        ledger: &Ledger,
        arena: &mut Arena,
        heap: &PagedHeap,
    ) -> Result<Option<logical::Prepared>, doc::heap::HeapFault> {
        if !ledger.is_current(&self.stamp) {
            return Ok(None);
        }
        let inputs: Vec<_> = self
            .region
            .entries
            .iter()
            .map(|entry| self.invocation.owner(entry.slot))
            .collect();
        let outputs: Vec<_> = self
            .region
            .survivors
            .iter()
            .map(|(slot, live)| {
                (
                    self.invocation.owner(*slot),
                    RootValue::Value(&self.values[live.fact.0]),
                )
            })
            .collect();
        let before = ledger.balance()?.total()?;
        let prepared = ledger.prepare_source_ownership(arena, heap, &inputs, &outputs)?;
        let after = prepared.balance()?.total()?;
        if i128::from(after) - i128::from(before) != self.delta {
            return Ok(None);
        }
        Ok(prepared.require_source_room(self.peak))
    }
}
