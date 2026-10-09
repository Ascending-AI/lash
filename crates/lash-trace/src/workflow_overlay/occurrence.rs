//! The observations of one identity space (an execution, or one occurrence
//! of a site), the rule that settles two that disagree, and what an
//! occurrence's observations fold to at its site.

use super::*;

/// One observation as the reducer holds it. Its identity is its key in the
/// [`Observations`] that hold it.
pub(super) struct Observation {
    pub(super) timestamp: DateTime<Utc>,
    pub(super) fact: WorkflowOverlayFact,
}

impl Observation {
    /// The digest a conflict lists this observation under.
    fn variant(&self) -> String {
        #[derive(Serialize)]
        struct Variant<'a> {
            timestamp: DateTime<Utc>,
            #[serde(flatten)]
            fact: &'a WorkflowOverlayFact,
        }
        #[expect(
            clippy::expect_used,
            reason = "the reducer only serializes its own infallible in-memory trace value types"
        )]
        let bytes = serde_json::to_vec(&Variant {
            timestamp: self.timestamp,
            fact: &self.fact,
        })
        .expect("workflow overlay observations serialize");
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    fn finish_status(&self) -> Option<LanguageExecutionStatus> {
        match &self.fact {
            WorkflowOverlayFact::Language {
                payload: TraceLanguageExecutionPayload::ExecutionFinished { status, .. },
                ..
            } => Some(*status),
            _ => None,
        }
    }
}

struct Held {
    observation: Observation,
    /// The smallest and the largest variant observed, when two disagreed.
    variants: Option<(String, String)>,
}

impl Held {
    /// The one rule for two different observations of one identity. A
    /// finish that ends the execution outranks one that leaves it running.
    /// Otherwise the earlier observation stays, and of two at one instant
    /// the one with the smaller variant: the choice never depends on the
    /// order they were folded in.
    fn resolve(&mut self, candidate: Observation) {
        if self.observation.timestamp == candidate.timestamp
            && self.observation.fact == candidate.fact
        {
            return;
        }
        let (held, offered) = (self.observation.variant(), candidate.variant());
        let ends = |observation: &Observation| {
            observation
                .finish_status()
                .is_some_and(LanguageExecutionStatus::is_terminal)
        };
        let keep_held = match (ends(&self.observation), ends(&candidate)) {
            (true, false) => true,
            (false, true) => false,
            _ => (self.observation.timestamp, &held) <= (candidate.timestamp, &offered),
        };
        let (low, high) = self.variants.take().unwrap_or_else(|| (held.clone(), held));
        self.variants = Some((
            low.min(offered.clone()),
            if high >= offered { high } else { offered },
        ));
        if !keep_held {
            self.observation = candidate;
        }
    }
}

/// The observations of one identity space, in identity order.
pub(super) struct Observations<K>(BTreeMap<K, Held>);

impl<K> Default for Observations<K> {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

impl<K: Ord> Observations<K> {
    pub(super) fn insert(&mut self, key: K, observation: Observation) {
        match self.0.entry(key) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(Held {
                    observation,
                    variants: None,
                });
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                entry.get_mut().resolve(observation);
            }
        }
    }

    pub(super) fn get(&self, key: &K) -> Option<&Observation> {
        self.0.get(key).map(|held| &held.observation)
    }

    pub(super) fn clear(&mut self) {
        self.0.clear();
    }

    fn iter(&self) -> impl Iterator<Item = (&K, &Observation)> {
        self.0.iter().map(|(key, held)| (key, &held.observation))
    }

    /// Record the variants a persisted conflict listed for `key`.
    fn restore_variants(&mut self, key: &K, variants: &[String]) {
        if let (Some(held), Some(low), Some(high)) =
            (self.0.get_mut(key), variants.first(), variants.last())
        {
            held.variants = Some((low.clone(), high.clone()));
        }
    }

    /// Append the published history of these observations, and the
    /// conflicts among them when `conflicts` is given. `identity` names the
    /// observation at each key.
    fn publish(
        &self,
        mut identity: impl FnMut(&K, &Observation) -> Option<WorkflowOverlayEventIdentity>,
        history: &mut Vec<WorkflowOverlayHistoryEvent>,
        mut conflicts: Option<&mut Vec<WorkflowOverlayConflict>>,
    ) {
        for (key, held) in &self.0 {
            meter::count();
            let Some(identity) = identity(key, &held.observation) else {
                continue;
            };
            if let (Some(conflicts), Some((low, high))) = (conflicts.as_mut(), &held.variants) {
                conflicts.push(WorkflowOverlayConflict {
                    identity: identity.clone(),
                    kind: WorkflowOverlayConflictKind::ConflictingDuplicate,
                    variants: vec![low.clone(), high.clone()],
                });
            }
            history.push(WorkflowOverlayHistoryEvent {
                identity,
                timestamp: held.observation.timestamp,
                fact: held.observation.fact.clone(),
            });
        }
    }
}

pub(super) type ExecutionHistory = Observations<WorkflowOverlayExecutionTransition>;

impl ExecutionHistory {
    pub(super) fn publish_execution(
        &self,
        history: &mut Vec<WorkflowOverlayHistoryEvent>,
        conflicts: &mut Vec<WorkflowOverlayConflict>,
    ) {
        self.publish(
            |transition, _| {
                Some(WorkflowOverlayEventIdentity::Execution {
                    transition: *transition,
                })
            },
            history,
            Some(conflicts),
        );
    }

    pub(super) fn restore_conflict(
        &mut self,
        transition: WorkflowOverlayExecutionTransition,
        variants: &[String],
    ) {
        self.restore_variants(&transition, variants);
    }

    /// The document the execution's start named.
    pub(super) fn started_document(&self) -> Option<&WorkflowDocumentRef> {
        match &self.get(&WorkflowOverlayExecutionTransition::Started)?.fact {
            WorkflowOverlayFact::Language {
                document,
                payload: TraceLanguageExecutionPayload::ExecutionStarted,
            } => Some(document),
            _ => None,
        }
    }

    /// The status the execution's own finish reported.
    pub(super) fn finished_status(&self) -> Option<LanguageExecutionStatus> {
        self.get(&WorkflowOverlayExecutionTransition::Finished)?
            .finish_status()
    }
}

/// Which observation of an occurrence one is. A wait and a resume are keyed
/// by when they were observed, so an occurrence holds as many as it made;
/// their published ordinals are their ranks in that order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Transition {
    Started,
    StepBody(u32),
    Waiting(DateTime<Utc>),
    Resumed(DateTime<Utc>),
    Terminal,
    BranchSelected,
    ChildStarted,
}

/// Where an observation belongs: the identity it is folded under.
pub(super) enum Placement {
    Execution(WorkflowOverlayExecutionTransition),
    Occurrence {
        site: WorkflowSiteRef,
        occurrence: u64,
        transition: Transition,
    },
}

impl Placement {
    pub(super) fn of(observation: &Observation) -> Self {
        use TraceLanguageExecutionPayload as Payload;
        let (at, transition) = match &observation.fact {
            WorkflowOverlayFact::StepBodyStarted { step } => {
                (&step.at, Transition::StepBody(step.attempt))
            }
            WorkflowOverlayFact::Language { payload, .. } => match payload {
                Payload::ExecutionStarted => {
                    return Self::Execution(WorkflowOverlayExecutionTransition::Started);
                }
                Payload::ExecutionFinished { .. } => {
                    return Self::Execution(WorkflowOverlayExecutionTransition::Finished);
                }
                Payload::Node { at, fact } => (
                    at,
                    match fact {
                        TraceNodeFact::Started { .. } => Transition::Started,
                        TraceNodeFact::Waiting { .. } => Transition::Waiting(observation.timestamp),
                        TraceNodeFact::Resumed { .. } => Transition::Resumed(observation.timestamp),
                        TraceNodeFact::Completed { .. }
                        | TraceNodeFact::Failed { .. }
                        | TraceNodeFact::Cancelled => Transition::Terminal,
                        TraceNodeFact::BranchSelected { .. } => Transition::BranchSelected,
                        TraceNodeFact::ChildStarted { .. } => Transition::ChildStarted,
                    },
                ),
            },
        };
        Self::Occurrence {
            site: at.site.clone(),
            occurrence: at.occurrence.get(),
            transition,
        }
    }
}

impl Observation {
    /// The occurrence the observation is about, loops and all; `None` for a
    /// fact about the whole execution.
    fn at(&self) -> Option<&lash_sansio::WorkflowOccurrence> {
        match &self.fact {
            WorkflowOverlayFact::StepBodyStarted { step } => Some(&step.at),
            WorkflowOverlayFact::Language { payload, .. } => payload.at(),
        }
    }
}

pub(super) type OccurrenceHistory = Observations<Transition>;

impl OccurrenceHistory {
    /// Publish the occurrence's observations, each at the occurrence its own
    /// fact names.
    fn publish_occurrence(
        &self,
        history: &mut Vec<WorkflowOverlayHistoryEvent>,
        conflicts: Option<&mut Vec<WorkflowOverlayConflict>>,
    ) {
        let (mut waits, mut resumes) = (0, 0);
        self.publish(
            |transition, observation| {
                use WorkflowOverlayNodeTransition as Node;
                let at = observation.at()?.clone();
                let transition = match transition {
                    Transition::StepBody(attempt) => {
                        return Some(WorkflowOverlayEventIdentity::StepBody {
                            at,
                            attempt: *attempt,
                        });
                    }
                    Transition::Started => Node::Started,
                    Transition::Waiting(_) => {
                        waits += 1;
                        Node::Waiting { ordinal: waits }
                    }
                    Transition::Resumed(_) => {
                        resumes += 1;
                        Node::Resumed { ordinal: resumes }
                    }
                    Transition::Terminal => Node::Terminal,
                    Transition::BranchSelected => Node::BranchSelected,
                    Transition::ChildStarted => Node::ChildStarted,
                };
                Some(WorkflowOverlayEventIdentity::Node { at, transition })
            },
            history,
            conflicts,
        );
    }

    /// The published history of a retained occurrence and its conflicts.
    pub(super) fn publish_retained(
        &self,
        history: &mut Vec<WorkflowOverlayHistoryEvent>,
        conflicts: &mut Vec<WorkflowOverlayConflict>,
    ) {
        self.publish_occurrence(history, Some(conflicts));
    }

    /// The published history of an evicted occurrence. Its conflicts left
    /// the overlay with it.
    pub(super) fn publish_evicted(&self) -> Vec<WorkflowOverlayHistoryEvent> {
        let mut history = Vec::new();
        self.publish_occurrence(&mut history, None);
        history
    }

    /// Record the variants a persisted conflict listed for `identity`.
    pub(super) fn restore_conflict(
        &mut self,
        identity: &WorkflowOverlayEventIdentity,
        variants: &[String],
    ) {
        use WorkflowOverlayNodeTransition as Node;
        let nth = |ordinal: u32, wait: bool| {
            self.0
                .keys()
                .filter(|key| match key {
                    Transition::Waiting(_) => wait,
                    Transition::Resumed(_) => !wait,
                    _ => false,
                })
                .nth(ordinal.saturating_sub(1) as usize)
                .copied()
        };
        let key = match identity {
            WorkflowOverlayEventIdentity::Execution { .. } => None,
            WorkflowOverlayEventIdentity::StepBody { attempt, .. } => {
                Some(Transition::StepBody(*attempt))
            }
            WorkflowOverlayEventIdentity::Node { transition, .. } => match transition {
                Node::Started => Some(Transition::Started),
                Node::Waiting { ordinal } => nth(*ordinal, true),
                Node::Resumed { ordinal } => nth(*ordinal, false),
                Node::Terminal => Some(Transition::Terminal),
                Node::BranchSelected => Some(Transition::BranchSelected),
                Node::ChildStarted => Some(Transition::ChildStarted),
            },
        };
        if let Some(key) = key {
            self.restore_variants(&key, variants);
        }
    }

    /// What the occurrence's observations say of it.
    fn fold(&self) -> OccurrenceFold<'_> {
        meter::count();
        let mut folded = OccurrenceFold::default();
        for (transition, observation) in self.iter() {
            let timestamp = observation.timestamp;
            folded.counted |= *transition != Transition::ChildStarted;
            match &observation.fact {
                WorkflowOverlayFact::StepBodyStarted { step } => {
                    folded.started(timestamp);
                    folded.call = Some((&step.call_id, Some(step.attempt)));
                }
                WorkflowOverlayFact::Language { payload, .. } => match payload.node_fact() {
                    None => {}
                    Some(TraceNodeFact::Started { call_id }) => {
                        folded.started(timestamp);
                        folded.repeat_call(call_id.as_ref());
                    }
                    // Waits and resumes arrive in time order: the last of
                    // each is the latest.
                    Some(TraceNodeFact::Waiting { awaited }) => {
                        folded.waiting = Some((timestamp, awaited));
                    }
                    Some(TraceNodeFact::Resumed { .. }) => folded.resumed = Some(timestamp),
                    Some(TraceNodeFact::Completed { call_id }) => {
                        folded.terminal = Some(OccurrenceTerminal::Completed(timestamp));
                        folded.repeat_call(call_id.as_ref());
                    }
                    Some(TraceNodeFact::Failed { call_id, failure }) => {
                        folded.terminal = Some(OccurrenceTerminal::Failed(timestamp, failure));
                        folded.repeat_call(call_id.as_ref());
                    }
                    Some(TraceNodeFact::Cancelled) => {
                        folded.terminal = Some(OccurrenceTerminal::Cancelled(timestamp));
                    }
                    // A selection ends its branch unless the branch's own
                    // terminal, which sorts first, already did.
                    Some(TraceNodeFact::BranchSelected { selected }) => {
                        folded.branch = Some(*selected);
                        folded
                            .terminal
                            .get_or_insert(OccurrenceTerminal::Completed(timestamp));
                    }
                    Some(TraceNodeFact::ChildStarted { child }) => folded.children.push(child),
                },
            }
        }
        folded
    }
}

#[derive(Default)]
struct OccurrenceFold<'a> {
    /// Whether anything but a child link was observed: a child link is a
    /// fact about the site, not a transition of one of its occurrences.
    counted: bool,
    start: Option<DateTime<Utc>>,
    waiting: Option<(DateTime<Utc>, &'a crate::TraceNodeAwaited)>,
    resumed: Option<DateTime<Utc>>,
    terminal: Option<OccurrenceTerminal<'a>>,
    call: Option<(&'a lash_sansio::ToolCallId, Option<u32>)>,
    branch: Option<TraceBranchSelection>,
    children: Vec<&'a TraceLanguageChildExecution>,
}

#[derive(Clone, Copy)]
enum OccurrenceTerminal<'a> {
    Completed(DateTime<Utc>),
    Failed(DateTime<Utc>, &'a TraceLanguageExecutionFailure),
    Cancelled(DateTime<Utc>),
}

impl<'a> OccurrenceFold<'a> {
    /// The occurrence began no later than `timestamp`: a retried body starts
    /// again, and the occurrence still began at its first start.
    fn started(&mut self, timestamp: DateTime<Utc>) {
        self.start = Some(self.start.map_or(timestamp, |start| start.min(timestamp)));
    }

    /// Bind the occurrence to the call a language fact repeated, unless the
    /// step's actor stated the binding (it carries the attempt).
    fn repeat_call(&mut self, call_id: Option<&'a lash_sansio::ToolCallId>) {
        if let Some(call_id) = call_id
            && !matches!(self.call, Some((_, Some(_))))
        {
            self.call = Some((call_id, None));
        }
    }

    fn in_flight(&self) -> bool {
        self.start.is_some() || self.waiting.is_some()
    }

    /// How the occurrence ended. Only one observed in flight can be
    /// cancelled, by its own fact or by the process's committed
    /// cancellation at `cancelled_at`.
    fn ended(&self, cancelled_at: Option<DateTime<Utc>>) -> Option<OccurrenceTerminal<'a>> {
        match self.terminal {
            Some(OccurrenceTerminal::Cancelled(_)) if !self.in_flight() => None,
            Some(terminal) => Some(terminal),
            None if self.in_flight() => cancelled_at.map(OccurrenceTerminal::Cancelled),
            None => None,
        }
    }

    fn state(
        &self,
        occurrence: u64,
        cancelled_at: Option<DateTime<Utc>>,
    ) -> WorkflowOverlayOccurrence {
        let start = self.start;
        match self.ended(cancelled_at) {
            Some(OccurrenceTerminal::Completed(end)) => WorkflowOverlayOccurrence::Completed {
                occurrence,
                start,
                end,
            },
            Some(OccurrenceTerminal::Failed(end, failure)) => WorkflowOverlayOccurrence::Failed {
                occurrence,
                start,
                end,
                failure: failure.clone(),
            },
            Some(OccurrenceTerminal::Cancelled(end)) => WorkflowOverlayOccurrence::Cancelled {
                occurrence,
                start,
                end,
            },
            None => match (self.waiting, start) {
                (Some((since, awaited)), _)
                    if self.resumed.is_none_or(|resumed| resumed < since) =>
                {
                    WorkflowOverlayOccurrence::Waiting {
                        occurrence,
                        start,
                        since,
                        awaited: awaited.clone(),
                    }
                }
                (_, Some(start)) => WorkflowOverlayOccurrence::Running { occurrence, start },
                (_, None) => WorkflowOverlayOccurrence::Unobserved,
            },
        }
    }
}

/// A child execution's place among its site's links.
type ChildKey = (lash_sansio::ProcessId, Option<u32>);

/// The child executions one site started, each listed once.
#[derive(Clone, Default)]
pub(super) struct SiteChildren(BTreeMap<ChildKey, TraceLanguageChildExecution>);

impl SiteChildren {
    pub(super) fn of(children: &[TraceLanguageChildExecution]) -> Self {
        let mut links = Self::default();
        for child in children {
            links.insert(child);
        }
        links
    }

    pub(super) fn insert(&mut self, child: &TraceLanguageChildExecution) {
        self.0
            .insert((child.process_id.clone(), child.attempt), child.clone());
    }

    pub(super) fn iter(&self) -> impl Iterator<Item = &TraceLanguageChildExecution> {
        self.0.values()
    }
}

impl WorkflowOverlaySiteState {
    /// Fold `occurrences`, in occurrence order and all later than any the
    /// state already holds, onto the site. `cancelled_at` is when the
    /// process's committed cancellation ended what was still in flight.
    pub(super) fn fold<'a>(
        &mut self,
        occurrences: impl IntoIterator<Item = (u64, &'a OccurrenceHistory)>,
        cancelled_at: Option<DateTime<Utc>>,
        children: &mut SiteChildren,
    ) {
        let mut latest = None;
        for (occurrence, history) in occurrences {
            let folded = history.fold();
            for child in &folded.children {
                children.insert(child);
            }
            if !folded.counted {
                continue;
            }
            self.summary.retained_occurrences += 1;
            self.summary.started_count += u64::from(folded.start.is_some());
            if let Some(terminal) = folded.ended(cancelled_at) {
                let (status, end) = match terminal {
                    OccurrenceTerminal::Completed(end) => {
                        (WorkflowOverlayTerminalStatus::Completed, end)
                    }
                    OccurrenceTerminal::Failed(end, _) => {
                        (WorkflowOverlayTerminalStatus::Failed, end)
                    }
                    OccurrenceTerminal::Cancelled(end) => {
                        (WorkflowOverlayTerminalStatus::Cancelled, end)
                    }
                };
                self.summary.ended(WorkflowOverlayTerminalRecord {
                    occurrence,
                    status,
                    end,
                });
            }
            if let Some((call_id, attempt)) = folded.call {
                self.call = Some(WorkflowOverlayCall {
                    occurrence,
                    call_id: call_id.clone(),
                    attempt,
                });
            }
            if folded.branch.is_some() {
                self.branch = folded.branch;
            }
            latest = Some((occurrence, folded));
        }
        if let Some((occurrence, folded)) = latest {
            self.occurrence = folded.state(occurrence, cancelled_at);
        }
    }
}

impl WorkflowOverlaySiteReport {
    /// Count the terminal of an occurrence later than any counted before.
    pub(super) fn ended(&mut self, terminal: WorkflowOverlayTerminalRecord) {
        self.terminal_count += 1;
        self.first_terminal.get_or_insert_with(|| terminal.clone());
        self.last_terminal = Some(terminal);
    }
}

/// Counts the units of work the reducer does (one for each occurrence it
/// folds and each history event it publishes), so a law can hold the cost of
/// a snapshot and of an append to the size of what they touch.
pub(super) mod meter {
    #[cfg(test)]
    thread_local! {
        static WORK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    #[inline]
    pub(in super::super) fn count() {
        #[cfg(test)]
        WORK.with(|work| work.set(work.get() + 1));
    }

    /// The work done since the last call.
    #[cfg(test)]
    pub(in super::super) fn take() -> u64 {
        WORK.with(|work| work.replace(0))
    }
}
