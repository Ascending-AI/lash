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

use lash_durable::{ActorKey, CommitLabel, DurableError};
use lash_sansio::sync::MutexExt as _;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How one labelled write is cut.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
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

#[derive(Default)]
struct State {
    rules: Vec<Rule>,
    trace: Vec<Write>,
    cuts: Vec<Cut>,
}

#[derive(Default)]
pub(crate) struct Shared {
    state: Mutex<State>,
    recorded: tokio::sync::Notify,
}

/// A write about to enter the store, numbered and matched against the rules.
pub(crate) struct Entry {
    shared: Arc<Shared>,
    index: usize,
    pub(crate) fault: Option<Fault>,
}

impl Entry {
    /// Record what the store did with the write.
    pub(crate) fn finish(&self, stored: Stored) {
        self.shared.state.lock_recover().trace[self.index].stored = stored;
        self.shared.recorded.notify_waiters();
    }
}

impl Shared {
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

    /// Every write so far, in the order the writes entered.
    pub fn trace(&self) -> Vec<Write> {
        self.shared.state.lock_recover().trace.clone()
    }

    /// Every write a rule cut so far, in firing order.
    pub fn cuts(&self) -> Vec<Cut> {
        self.shared.state.lock_recover().cuts.clone()
    }

    /// Disarm every rule that has not fired, answering each, so a matrix
    /// reports an unreached cut instead of panicking on drop.
    pub fn disarm_unfired(&self) -> Vec<String> {
        let mut state = self.shared.state.lock_recover();
        let unfired = state
            .rules
            .iter()
            .filter(|rule| !rule.fired)
            .map(|rule| format!("`{rule}`"))
            .collect();
        state.rules.retain(|rule| rule.fired);
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
        let unfired: Vec<String> = {
            let state = self.shared.state.lock_recover();
            state
                .rules
                .iter()
                .filter(|rule| !rule.fired)
                .map(|rule| format!("`{rule}`"))
                .collect()
        };
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
