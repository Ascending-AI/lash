//! A selected source path compiled once into indexed logical algebra. Selection
//! validation walks source control only on a cache miss, never per execution.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Selection {
    Event { at: Point, exit: ExitKind },
    Branch { at: Site, taken: bool },
}
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Key {
    at: Site,
    selected: Box<[Selection]>,
}

struct Linear {
    fixed: i128,
    terms: Box<[(usize, i128)]>,
}
impl Linear {
    fn compile(amount: &Amount, variables: &BTreeMap<Measure, usize>) -> Self {
        Self {
            fixed: amount.fixed,
            terms: amount
                .terms
                .iter()
                .map(|(measure, coefficient)| (variables[measure], *coefficient))
                .collect(),
        }
    }
    fn evaluate(&self, facts: &[i128]) -> Result<i128, Refused> {
        self.terms
            .iter()
            .try_fold(self.fixed, |sum, (at, coefficient)| {
                sum.checked_add(facts[*at].checked_mul(*coefficient).ok_or(Refused::Facts)?)
                    .ok_or(Refused::Facts)
            })
    }
}
struct Step {
    event: Event,
    selected: ExitKind,
    normal: ExitKind,
    variables: Box<[(Measure, usize)]>,
    peak: Linear,
    delta: Linear,
}
/// The fields and constructors are private. A schedule is not a source proof;
/// only the independent Source verifier and control walker form this program.
pub(crate) struct CompiledInterval {
    owner: doc::constant_pool::PoolOwner,
    source: Site,
    at: Site,
    route: Box<[Selection]>,
    steps: Box<[Step]>,
    variables: usize,
    exit: ExitKind,
}
impl Source {
    pub(crate) fn compile_interval(
        &self,
        at: &Site,
        selected: &[Selection],
        budget: usize,
    ) -> Result<Arc<CompiledInterval>, Refused> {
        self.verify()?;
        if selected.len() > budget.min(4096) {
            return Err(Refused::Path);
        }
        let key = Key {
            at: at.clone(),
            selected: selected.into(),
        };
        let mut cache = self.intervals.lock().map_err(|_| Refused::Source)?;
        if let Some(interval) = cache.get(&key) {
            return Ok(Arc::clone(interval));
        }
        if cache.len() >= 128 {
            return Err(Refused::Path);
        }
        let mut trace = Trace {
            source: self,
            selected,
            cursor: 0,
            events: Vec::new(),
        };
        let exit = evaluate::walk(self, at, &mut trace, budget)?;
        if trace.cursor != selected.len() {
            return Err(Refused::Path);
        }
        let mut variables = 0;
        let mut steps = Vec::with_capacity(trace.events.len());
        for (event, selected) in trace.events {
            let indices: BTreeMap<_, _> = event
                .peak
                .terms
                .keys()
                .chain(event.delta.terms.keys())
                .cloned()
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .map(|measure| {
                    let index = variables;
                    variables += 1;
                    (measure, index)
                })
                .collect();
            steps.push(Step {
                peak: Linear::compile(&event.peak, &indices),
                delta: Linear::compile(&event.delta, &indices),
                normal: self
                    .room
                    .exits
                    .iter()
                    .find(|exit| exit.point == event.point)
                    .ok_or(Refused::Exit)?
                    .kind,
                variables: indices.into_iter().collect(),
                event,
                selected,
            });
        }
        let interval = Arc::new(CompiledInterval {
            owner: self.owner,
            source: self.site.clone(),
            at: at.clone(),
            route: selected.into(),
            steps: steps.into(),
            variables,
            exit,
        });
        cache.insert(key, Arc::clone(&interval));
        Ok(interval)
    }
}
struct Trace<'a> {
    source: &'a Source,
    selected: &'a [Selection],
    cursor: usize,
    events: Vec<(Event, ExitKind)>,
}
impl evaluate::Events for Trace<'_> {
    fn step(&mut self, site: &Site, phase: Phase) -> Result<ExitKind, Refused> {
        let expected = point(site, phase);
        let Some(Selection::Event { at, exit }) = self.selected.get(self.cursor) else {
            return Err(Refused::Path);
        };
        if *at != expected
            || !self
                .source
                .room
                .exits
                .iter()
                .any(|allowed| allowed.point == *at && allowed.kind == *exit)
        {
            return Err(Refused::Exit);
        }
        let event = self.source.room.peak_extra.get(at).ok_or(Refused::Path)?;
        self.events.push((event.clone(), *exit));
        self.cursor += 1;
        Ok(*exit)
    }
    fn choose(&mut self, site: &Site) -> Result<bool, Refused> {
        let Some(Selection::Branch { at, taken }) = self.selected.get(self.cursor) else {
            return Err(Refused::Path);
        };
        if at != site {
            return Err(Refused::Path);
        }
        self.cursor += 1;
        Ok(*taken)
    }
}
impl CompiledInterval {
    pub(crate) fn exit(&self) -> ExitKind {
        self.exit
    }
    pub(crate) fn start(&self) -> &Site {
        &self.at
    }
    pub(crate) fn events(&self) -> impl ExactSizeIterator<Item = &Event> {
        self.steps.iter().map(|step| &step.event)
    }
    /// Bind canonical event facts once, then evaluate indexed polynomials.
    /// No source node, child vector, or AST traversal is used here.
    pub(crate) fn evaluate_interval(
        &self,
        execution: &mut impl Execution,
    ) -> Result<CheckedRoom, Refused> {
        let mut values = vec![0; self.variables];
        let mut balance = 0_i128;
        let mut peak = 0_i128;
        let mut steps = self.steps.iter();
        for selection in &self.route {
            match selection {
                Selection::Branch { at, taken } => {
                    if execution.choose(at) != Some(*taken) {
                        return Err(Refused::Path);
                    }
                }
                Selection::Event { at, .. } => {
                    let step = steps.next().ok_or(Refused::Path)?;
                    let facts = execution.event(at).ok_or(Refused::Facts)?;
                    for (measure, index) in &step.variables {
                        values[*index] =
                            Amount::measure(measure, facts.as_ref()).ok_or(Refused::Facts)?;
                    }
                    let actual = event_exit(&step.event, facts.as_ref(), step.normal)?;
                    if actual != step.selected {
                        return Err(Refused::Exit);
                    }
                    let extra = step.peak.evaluate(&values)?;
                    if extra < 0 {
                        return Err(Refused::Peak);
                    }
                    peak = peak.max(balance.checked_add(extra).ok_or(Refused::Facts)?);
                    balance = balance
                        .checked_add(step.delta.evaluate(&values)?)
                        .ok_or(Refused::Facts)?;
                    peak = peak.max(balance);
                }
            }
        }
        Ok(CheckedRoom {
            owner: self.owner,
            native: false,
            whole: self.at == self.source,
            native_contract: None,
            balance: Balance {
                peak_extra: u64::try_from(peak.max(0)).map_err(|_| Refused::Facts)?,
                retained_delta: balance,
            },
            exit: self.exit,
        })
    }
}
fn event_exit(event: &Event, facts: &dyn Facts, normal: ExitKind) -> Result<ExitKind, Refused> {
    if facts.raised() {
        if event.output.is_none() {
            return Err(Refused::Facts);
        }
        return Ok(ExitKind::Raise);
    }
    for measure in event.peak.terms.keys() {
        if let Measure::CallPeak { at, callee } = measure {
            return Ok(
                if facts.call(at, *callee).ok_or(Refused::Facts)?.exit == ExitKind::Raise {
                    ExitKind::Raise
                } else {
                    ExitKind::Normal
                },
            );
        }
    }
    Ok(normal)
}
