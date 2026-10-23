//! The fault script: which labelled write is cut, how, and what every write
//! did.
//!
//! Every write a node makes through its [`FaultStore`](crate::FaultStore)
//! carries a [`CommitLabel`]: the caller's label for an actor or mailbox
//! commit, and the port's fixed label for a claim, heartbeat, reap, node
//! registration or release. A rule names one write by label and occurrence
//! (counted from 1 across every node, or among one node's own writes) and the
//! [`Fault`] it suffers. Every write is recorded, so a law asserts what was
//! cut, and a rule that never fired fails the law when its script drops.
//! A read rule fails one read of an actor's row the same way.

use lash_durable::domain::{AdmittedId, OwnerKey, RunFacts};
use lash_durable::{ActorKey, CommitLabel, DurableError, Epoch, OwnerCut};
use lash_protocol_types::sync::MutexExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How one labelled write is cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Fault {
    /// The caller is answered a transient store failure before the write
    /// enters the store: nothing it carried is durable, and the node lives.
    FailBefore,
    /// The node dies before the write enters the store: nothing it carried
    /// is durable, and none of the node's tasks runs again.
    Abort,
    /// The write commits, then the node dies before the caller sees the
    /// reply.
    CommitThenAbort,
    /// The write commits and the caller is answered
    /// [`DurableError::AckLost`](lash_durable::DurableError::AckLost).
    AckHidden,
    /// The write commits and its reply reaches the caller after this much
    /// virtual time.
    DelayedAck(Duration),
    /// The write commits, then the whole node pauses (its store calls and
    /// its timers) until its actors are reaped and claimed elsewhere; then
    /// the caller sees the reply and carries on under a stale epoch.
    StaleEpoch,
    /// The whole node pauses before the write enters the store, until its
    /// actors are reaped and claimed elsewhere; then the paused write enters
    /// the store, as a zombie's.
    Zombie,
    /// A mailbox write commits, and the wake it would announce to the
    /// woken actors' owners is lost: only the owners' own polls find it.
    LostWake,
}

impl Fault {
    /// Whether the write's effects reach the store under this fault, absent
    /// the store's own refusal.
    pub const fn commits(self) -> bool {
        !matches!(self, Self::FailBefore | Self::Abort | Self::Zombie)
    }

    /// Whether this fault pauses the node until its actors move elsewhere.
    pub const fn pauses(self) -> bool {
        matches!(self, Self::StaleEpoch | Self::Zombie)
    }

    /// Whether this fault kills the node.
    pub const fn kills(self) -> bool {
        matches!(self, Self::Abort | Self::CommitThenAbort)
    }
}

impl std::fmt::Display for Fault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FailBefore => f.write_str("fail-before"),
            Self::Abort => f.write_str("abort"),
            Self::CommitThenAbort => f.write_str("commit-then-abort"),
            Self::AckHidden => f.write_str("ack-hidden"),
            Self::DelayedAck(delay) => write!(f, "delayed-ack({}ms)", delay.as_millis()),
            Self::StaleEpoch => f.write_str("stale-epoch"),
            Self::Zombie => f.write_str("zombie"),
            Self::LostWake => f.write_str("lost-wake"),
        }
    }
}

/// Fault after a successful durable acknowledgement. Neither form rolls back
/// storage; the owner must invalidate memory and recover the committed cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerFault {
    Fail,
    Panic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OwnerCutReceipt {
    pub actor: ActorKey,
    pub epoch: Epoch,
    pub label: CommitLabel,
    pub cut: OwnerCut,
    pub fault: Option<OwnerFault>,
}

struct OwnerRule {
    label: CommitLabel,
    nth: usize,
    cut: OwnerCut,
    fault: OwnerFault,
    fired: bool,
}

/// Which kind of port write a label was recorded on, so a matrix offers each
/// write only the faults that mean something there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WriteKind {
    /// An owner's fenced commit.
    Actor,
    /// A non-owner's mailbox commit.
    Mail,
    /// A node-lease write: claim, heartbeat, reap, registration or release.
    Lease,
}

/// One labelled write, by label and occurrence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Point {
    pub label: CommitLabel,
    /// The write's position among every node's writes under `label`, from 1.
    pub nth: usize,
}

impl std::fmt::Display for Point {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{}", self.label.as_str(), self.nth)
    }
}

/// What the store did with one write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stored {
    /// The write is still in the store, or held by a pause before it.
    Pending,
    /// A cut kept the write out of the store.
    NotEntered,
    /// The store committed it. `effective` is false for a lease write that
    /// changed nothing: a claim that took no actor, a reap that found no
    /// dead node.
    Committed { effective: bool },
    /// The store refused it, a fence refusal included; nothing is written.
    Refused(DurableError),
}

/// One write through a fault store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Write {
    pub node: Arc<str>,
    pub kind: WriteKind,
    pub point: Point,
    /// The write's position among `node`'s own writes under the label.
    pub node_nth: usize,
    /// The actor an owner commit was fenced on.
    pub actor: Option<ActorKey>,
    /// Virtual time when the write was made.
    pub at_ms: u64,
    /// The fault a rule cut it with.
    pub cut: Option<Fault>,
    pub stored: Stored,
    /// The `x_start` records an owner commit appends: the admitted
    /// executions it authorizes once committed. A later commit may prune
    /// the records, but never this.
    pub starts: Vec<AdmittedId>,
    /// The run facts an owner commit appends, with the nodes it stages: once
    /// committed, what its owners' ledgers hold. A turn's terminal retires
    /// its stored ledger (FIG-6601), never this.
    pub facts: Arc<RunFacts>,
}

impl Write {
    /// Whether the write's effects are in the store.
    pub fn committed(&self) -> bool {
        matches!(self.stored, Stored::Committed { .. })
    }
}

impl std::fmt::Display for Write {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}@{}", self.node, self.point, self.at_ms)?;
        if let Some(actor) = &self.actor {
            write!(f, "[{actor}]")?;
        }
        if let Some(cut) = self.cut {
            write!(f, " cut({cut})")?;
        }
        match &self.stored {
            Stored::Pending => f.write_str(" pending"),
            Stored::NotEntered => f.write_str(" not-entered"),
            Stored::Committed { effective: true } => f.write_str(" committed"),
            Stored::Committed { effective: false } => f.write_str(" no-op"),
            Stored::Refused(error) => write!(f, " refused({error})"),
        }
    }
}

/// A write a rule cut, as the trace recorded it when the cut fired.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cut {
    pub node: Arc<str>,
    pub kind: WriteKind,
    pub point: Point,
    pub fault: Fault,
}

impl std::fmt::Display for Cut {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} {}", self.node, self.point, self.fault)
    }
}

struct Rule {
    label: CommitLabel,
    nth: usize,
    node: Option<Arc<str>>,
    fault: Fault,
    fired: bool,
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(node) = &self.node {
            write!(f, "{node}:")?;
        }
        write!(f, "{}#{} {}", self.label.as_str(), self.nth, self.fault)
    }
}

/// A read rule: the next read of `actor`'s row fails transiently.
struct ReadRule {
    actor: ActorKey,
    fired: bool,
}

impl std::fmt::Display for ReadRule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "read of {} fail-before", self.actor)
    }
}

#[derive(Default)]
struct State {
    owner_rules: Vec<OwnerRule>,
    owner_cuts: Vec<OwnerCutReceipt>,
    rules: Vec<Rule>,
    reads: Vec<ReadRule>,
    trace: Vec<Write>,
    cuts: Vec<Cut>,
}

impl State {
    /// Every rule that has not fired, rendered.
    fn unfired(&self) -> Vec<String> {
        self.owner_rules
            .iter()
            .filter(|rule| !rule.fired)
            .map(|rule| {
                format!(
                    "{}#{}/{:?} {:?}",
                    rule.label.as_str(),
                    rule.nth,
                    rule.cut,
                    rule.fault
                )
            })
            .chain(
                self.rules
                    .iter()
                    .filter(|rule| !rule.fired)
                    .map(|rule| format!("`{rule}`"))
                    .chain(
                        self.reads
                            .iter()
                            .filter(|rule| !rule.fired)
                            .map(|rule| format!("`{rule}`")),
                    ),
            )
            .collect()
    }
}

type CommitRefusal = Arc<
    dyn Fn(CommitLabel, &[lash_durable::DomainWrite]) -> Option<lash_durable::DomainRefusal>
        + Send
        + Sync,
>;

/// What a law observes of every owner commit before it enters the store:
/// its label and its domain writes.
pub type CommitObserver = Arc<dyn Fn(CommitLabel, &lash_durable::ActorTx) + Send + Sync>;

/// What a law observes of every candidate publication before it enters
/// the store: its label, the manifest's summary and the fresh pages the
/// manifest supplies.
pub type CandidateObserver = Arc<
    dyn Fn(
            CommitLabel,
            lash_core_store::checkpoint_graph::stage::ManifestSummary,
            &[lash_core_store::checkpoint_graph::EncodedNode],
        ) + Send
        + Sync,
>;

type ActorReceiptObserver =
    Arc<dyn Fn(CommitLabel, &lash_durable::ActorTx, &lash_durable::ActorCommit) + Send + Sync>;
type MailReceiptObserver = Arc<dyn Fn(CommitLabel, &lash_durable::MailCommit) + Send + Sync>;
type ActorReplyObserver = Arc<
    dyn Fn(CommitLabel, &ActorKey, &Result<lash_durable::ActorCommit, DurableError>) + Send + Sync,
>;

#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    recorded: tokio::sync::Notify,
    observer: Mutex<Option<CommitObserver>>,
    candidate: Mutex<Option<CandidateObserver>>,
    refusal: Mutex<Option<CommitRefusal>>,
    pub(crate) actor_receipt: Mutex<Option<ActorReceiptObserver>>,
    pub(crate) mail_receipt: Mutex<Option<MailReceiptObserver>>,
    pub(crate) actor_reply: Mutex<Option<ActorReplyObserver>>,
}

/// A write about to enter the store, numbered and matched against the rules.
pub(crate) struct Entry {
    shared: Arc<Shared>,
    index: usize,
    pub(crate) fault: Option<Fault>,
}

impl Entry {
    /// Record the `x_start` records and the run facts the write appends.
    pub(crate) fn carrying(&self, starts: Vec<AdmittedId>, facts: Arc<RunFacts>) {
        let mut state = self.shared.state.lock_recover();
        let write = &mut state.trace[self.index];
        write.starts = starts;
        write.facts = facts;
    }

    /// Record what the store did with the write.
    pub(crate) fn finish(&self, stored: Stored) {
        self.shared.state.lock_recover().trace[self.index].stored = stored;
        self.shared.recorded.notify_waiters();
    }
}

impl Shared {
    pub(crate) fn owner_cut(
        &self,
        actor: &ActorKey,
        epoch: Epoch,
        label: CommitLabel,
        cut: OwnerCut,
    ) -> Result<(), DurableError> {
        let fault = {
            let mut state = self.state.lock_recover();
            let nth = 1 + state
                .owner_cuts
                .iter()
                .filter(|receipt| receipt.label == label && receipt.cut == cut)
                .count();
            let fault = state
                .owner_rules
                .iter_mut()
                .find(|rule| {
                    !rule.fired && rule.label == label && rule.cut == cut && rule.nth == nth
                })
                .map(|rule| {
                    rule.fired = true;
                    rule.fault
                });
            state.owner_cuts.push(OwnerCutReceipt {
                actor: actor.clone(),
                epoch,
                label,
                cut,
                fault,
            });
            fault
        };
        self.recorded.notify_waiters();
        match fault {
            None => Ok(()),
            Some(OwnerFault::Fail) => Err(DurableError::AckLost { label }),
            Some(OwnerFault::Panic) => panic!("owner cut {label:?}/{cut:?}"),
        }
    }

    pub(crate) fn refusal(
        &self,
        label: CommitLabel,
        writes: &[lash_durable::DomainWrite],
    ) -> Option<lash_durable::DomainRefusal> {
        let refusal = self.refusal.lock_recover().clone();
        refusal.and_then(|refusal| refusal(label, writes))
    }

    /// Hand an owner commit to the law observing them, if any: its domain
    /// writes and the graph nodes it stages.
    pub(crate) fn observe(&self, label: CommitLabel, tx: &lash_durable::ActorTx) {
        let observer = self.observer.lock_recover().clone();
        if let Some(observer) = observer {
            observer(label, tx);
        }
    }

    /// Hand a candidate's fresh pages to the law observing them, if any,
    /// reading its manifest once through; the store reads it again from
    /// its start.
    pub(crate) fn observe_candidate(
        &self,
        label: CommitLabel,
        manifest: &mut dyn lash_core_store::checkpoint_graph::stage::CandidateManifest,
    ) {
        use lash_core_store::checkpoint_graph::stage::ManifestEntry;
        let observer = self.candidate.lock_recover().clone();
        let Some(observer) = observer else {
            return;
        };
        let summary = manifest.summary();
        let mut fresh = Vec::new();
        let mut after = None;
        while let Ok(entries) = manifest.entries(after, 256) {
            let Some(last) = entries.last() else {
                break;
            };
            after = Some(match last {
                ManifestEntry::External(id) => *id,
                ManifestEntry::Fresh(node) => node.id,
            });
            fresh.extend(entries.into_iter().filter_map(|entry| match entry {
                ManifestEntry::Fresh(node) => Some(node.into_node()),
                ManifestEntry::External(_) => None,
            }));
        }
        observer(label, summary, &fresh);
    }

    /// Number one write, record it, and answer the fault an armed rule
    /// gives it.
    pub(crate) fn enter(
        self: &Arc<Self>,
        node: &Arc<str>,
        kind: WriteKind,
        label: CommitLabel,
        actor: Option<&ActorKey>,
        at_ms: u64,
    ) -> Entry {
        let (index, fault) = {
            let mut state = self.state.lock_recover();
            let nth = 1 + state
                .trace
                .iter()
                .filter(|write| write.point.label == label)
                .count();
            let node_nth = 1 + state
                .trace
                .iter()
                .filter(|write| write.point.label == label && write.node == *node)
                .count();
            let point = Point { label, nth };
            let fault = state
                .rules
                .iter_mut()
                .find(|rule| {
                    !rule.fired
                        && rule.label == label
                        && match &rule.node {
                            Some(only) => *only == *node && rule.nth == node_nth,
                            None => rule.nth == nth,
                        }
                })
                .map(|rule| {
                    rule.fired = true;
                    rule.fault
                });
            if let Some(fault) = fault {
                state.cuts.push(Cut {
                    node: Arc::clone(node),
                    kind,
                    point,
                    fault,
                });
            }
            state.trace.push(Write {
                node: Arc::clone(node),
                kind,
                point,
                node_nth,
                actor: actor.cloned(),
                at_ms,
                cut: fault,
                stored: Stored::Pending,
                starts: Vec::new(),
                facts: Arc::default(),
            });
            (state.trace.len() - 1, fault)
        };
        self.recorded.notify_waiters();
        Entry {
            shared: Arc::clone(self),
            index,
            fault,
        }
    }

    /// Whether an armed read rule fails this read of `actor`'s row; the
    /// rule fires.
    pub(crate) fn fails_read(&self, actor: &ActorKey) -> bool {
        let mut state = self.state.lock_recover();
        let rule = state
            .reads
            .iter_mut()
            .find(|rule| !rule.fired && rule.actor == *actor);
        rule.map(|rule| rule.fired = true).is_some()
    }

    fn rendered_trace(&self) -> String {
        let state = self.state.lock_recover();
        if state.trace.is_empty() {
            return "trace: (no writes)".into();
        }
        let writes: Vec<String> = state.trace.iter().map(Write::to_string).collect();
        format!("trace: {}", writes.join(", "))
    }
}

/// The rules a law armed and the writes its nodes made.
///
/// One script covers every node of a deployment: each node's
/// [`FaultStore`](crate::FaultStore) shares the script's rules and trace.
#[derive(Default)]
pub struct Script {
    pub(crate) shared: Arc<Shared>,
}

impl Script {
    /// Cut a named post-acknowledgement/adoption occurrence. Drop fails if
    /// the hook was never reached, just as for a database cut.
    pub fn cut_owner(
        &self,
        label: CommitLabel,
        nth: usize,
        cut: OwnerCut,
        fault: OwnerFault,
    ) -> &Self {
        assert!(nth > 0, "cuts are counted from 1");
        self.shared
            .state
            .lock_recover()
            .owner_rules
            .push(OwnerRule {
                label,
                nth,
                cut,
                fault,
                fired: false,
            });
        self
    }

    pub fn owner_cuts(&self) -> Vec<OwnerCutReceipt> {
        self.shared.state.lock_recover().owner_cuts.clone()
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Cut the `nth` write under `label` (counted across every node, from 1)
    /// with `fault`.
    pub fn cut(&self, label: CommitLabel, nth: usize, fault: Fault) -> &Self {
        self.arm(label, nth, None, fault)
    }

    /// Cut `node`'s own `nth` write under `label` with `fault`.
    pub fn cut_on(&self, node: &str, label: CommitLabel, nth: usize, fault: Fault) -> &Self {
        self.arm(label, nth, Some(Arc::from(node)), fault)
    }

    fn arm(&self, label: CommitLabel, nth: usize, node: Option<Arc<str>>, fault: Fault) -> &Self {
        assert!(nth > 0, "writes are counted from 1");
        self.shared.state.lock_recover().rules.push(Rule {
            label,
            nth,
            node,
            fault,
            fired: false,
        });
        self
    }

    /// Answer the next read of `actor`'s row, by any node, a transient
    /// store failure: nothing in the store changes, and the node lives.
    pub fn fail_actor_read(&self, actor: ActorKey) -> &Self {
        self.shared.state.lock_recover().reads.push(ReadRule {
            actor,
            fired: false,
        });
        self
    }

    /// Hand every owner commit's label, domain writes and staged nodes to
    /// `observer` before the commit enters the store, cut or not: what a
    /// law counts of what each commit submits.
    pub fn observe_commits(&self, observer: CommitObserver) -> &Self {
        *self.shared.observer.lock_recover() = Some(observer);
        self
    }

    /// Hand every candidate publication's label and fresh pages to
    /// `observer` before its owner commit is observed and enters the
    /// store, cut or not: the pages a commit publishes beside the nodes
    /// its transaction stages.
    pub fn observe_candidates(&self, observer: CandidateObserver) -> &Self {
        *self.shared.candidate.lock_recover() = Some(observer);
        self
    }

    /// Observe successful owner writes at the database boundary, before a
    /// fault can hide the receipt or kill its caller. The transaction is
    /// the one actually submitted; a refused or unentered write emits none.
    pub fn observe_actor_receipts(
        &self,
        observer: impl Fn(CommitLabel, &lash_durable::ActorTx, &lash_durable::ActorCommit)
        + Send
        + Sync
        + 'static,
    ) -> &Self {
        *self.shared.actor_receipt.lock_recover() = Some(Arc::new(observer));
        self
    }

    /// Observe accepted mail identities even when their caller loses the
    /// acknowledgement. Failed and unentered writes emit no receipt.
    pub fn observe_mail_receipts(
        &self,
        observer: impl Fn(CommitLabel, &lash_durable::MailCommit) + Send + Sync + 'static,
    ) -> &Self {
        *self.shared.mail_receipt.lock_recover() = Some(Arc::new(observer));
        self
    }

    /// Observe the answer an owner is about to receive, after reply faults.
    /// A killed caller emits none; a hidden acknowledgement emits AckLost.
    pub fn observe_actor_replies(
        &self,
        observer: impl Fn(CommitLabel, &ActorKey, &Result<lash_durable::ActorCommit, DurableError>)
        + Send
        + Sync
        + 'static,
    ) -> &Self {
        *self.shared.actor_reply.lock_recover() = Some(Arc::new(observer));
        self
    }

    /// Refuse selected owner commits at the store seam, before writing any
    /// rows. The trace records the typed refusal just as a real store does.
    pub fn refuse_commits(
        &self,
        refusal: impl Fn(
            CommitLabel,
            &[lash_durable::DomainWrite],
        ) -> Option<lash_durable::DomainRefusal>
        + Send
        + Sync
        + 'static,
    ) -> &Self {
        *self.shared.refusal.lock_recover() = Some(Arc::new(refusal));
        self
    }

    /// Every write so far, in the order the writes entered.
    pub fn trace(&self) -> Vec<Write> {
        self.shared.state.lock_recover().trace.clone()
    }

    /// `owner`'s ledger as the committed writes appended it, with every node
    /// they staged: what an ended turn's oracle folds once its terminal
    /// retired the stored ledger (FIG-6601).
    pub fn ledger(&self, owner: &OwnerKey) -> RunFacts {
        let mut ledger = RunFacts::default();
        for write in self
            .shared
            .state
            .lock_recover()
            .trace
            .iter()
            .filter(|write| write.committed())
        {
            ledger.extend(RunFacts::clone(&write.facts));
        }
        ledger.rows.retain(|row| &row.owner == owner);
        ledger
    }

    /// Every write a rule cut so far, in firing order.
    pub fn cuts(&self) -> Vec<Cut> {
        self.shared.state.lock_recover().cuts.clone()
    }

    /// Disarm every rule that has not fired, answering each, so a matrix
    /// reports an unreached cut instead of panicking on drop.
    pub fn disarm_unfired(&self) -> Vec<String> {
        let mut state = self.shared.state.lock_recover();
        let unfired = state.unfired();
        state.rules.retain(|rule| rule.fired);
        state.owner_rules.retain(|rule| rule.fired);
        state.reads.retain(|rule| rule.fired);
        unfired
    }

    /// The trace, rendered for a failure message.
    pub fn rendered_trace(&self) -> String {
        self.shared.rendered_trace()
    }

    /// Wait until `count` writes have left the store (or were kept out of
    /// it), whatever their callers have seen since.
    pub async fn settled(&self, count: usize) {
        loop {
            let recorded = self.shared.recorded.notified();
            let settled = self
                .shared
                .state
                .lock_recover()
                .trace
                .iter()
                .filter(|write| write.stored != Stored::Pending)
                .count();
            if settled >= count {
                return;
            }
            recorded.await;
        }
    }

    /// Wait until some rule has cut a write, and answer the first cut.
    pub async fn first_cut(&self) -> Cut {
        loop {
            let recorded = self.shared.recorded.notified();
            if let Some(cut) = self.shared.state.lock_recover().cuts.first() {
                return cut.clone();
            }
            recorded.await;
        }
    }
}

impl Drop for Script {
    /// Fails the law when a rule it armed never fired.
    fn drop(&mut self) {
        let unfired = self.shared.state.lock_recover().unfired();
        if !unfired.is_empty() && !std::thread::panicking() {
            panic!(
                "rule {} never fired; {}",
                unfired.join(", "),
                self.shared.rendered_trace()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A law cannot pass without exercising the cut it armed.
    #[test]
    #[should_panic(expected = "never fired")]
    fn a_rule_that_never_fired_fails_the_law() {
        let script = Script::new();
        script.cut(CommitLabel::new("t.never"), 1, Fault::FailBefore);
    }
}

#[cfg(test)]
mod owner_cut_laws {
    use super::*;
    use lash_durable::{DurableProbe, Epoch, OwnerCut};

    /// LO-RECOVERY: cuts carry occurrence evidence, and an unexercised
    /// selected memory cut fails the harness just like a database cut.
    #[test]
    fn lo_recovery_owner_cuts_require_occurrence_receipts() {
        let script = Script::new();
        let label = CommitLabel::TURN_COMMIT;
        script.cut_owner(label, 1, OwnerCut::Acknowledged, OwnerFault::Fail);
        script.cut_owner(label, 1, OwnerCut::Adopted, OwnerFault::Panic);
        let probe = crate::Tripwire::new();
        probe.observe_owner_cuts(&script);
        let actor = ActorKey::session("recovery-hook").expect("actor");
        assert!(matches!(
            probe.owner_cut(&actor, Epoch(1), label, OwnerCut::Acknowledged),
            Err(DurableError::AckLost { .. })
        ));
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _ = probe.owner_cut(&actor, Epoch(1), label, OwnerCut::Adopted);
            }))
            .is_err()
        );
        assert_eq!(script.owner_cuts().len(), 2);
        assert!(
            script
                .owner_cuts()
                .iter()
                .all(|receipt| receipt.fault.is_some())
        );
        assert_eq!(probe.counts().owner_cuts.values().sum::<usize>(), 2);
        drop(script);
        assert!(
            std::panic::catch_unwind(|| {
                let missing = Script::new();
                missing.cut_owner(label, 1, OwnerCut::Acknowledged, OwnerFault::Fail);
                drop(missing);
            })
            .is_err()
        );
    }
}
