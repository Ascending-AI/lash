//! Scripted faults and pauses over a store (ADR 0044 §Simulation).
//!
//! A law wraps a store in a [`Script`] and arms rules on the listed
//! operations: fail a call, lose its reply, hold it at a [`Gate`], or stop it
//! for good. The wrapper, [`Scripted`], is generated from the store and
//! deployment operation lists (`StoreOp`, `DeploymentOp`), so every listed
//! operation is scriptable and a new one joins with no edit here.
//!
//! A rule matches an operation's `nth` call, optionally of one actor, or the
//! calls of each of a set of operations ([`Script::on_each`]). There are no
//! predicates over arguments: a law that needs one overrides that operation
//! in a hand-written `RuntimeStoreDecorator`.
//!
//! A rule that never fired fails the law when its script drops, so a law
//! cannot pass without exercising what it armed.
use super::gate::{GATE_DEADLINE, Gate};
use crate::store::{MaintenanceFailure, RootIntentRefused, StoreError};
use lash_sansio::sync::MutexExt as _;
use std::future::Future;
use std::sync::{Arc, Mutex, Weak};

/// One listed store or deployment operation, by its name in the list.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Op(&'static str);

impl Op {
    /// The operation the list names `name`. Laws name operations through
    /// `StoreOp` and `DeploymentOp`, which convert to this.
    pub const fn listed(name: &'static str) -> Self {
        Self(name)
    }

    pub const fn name(self) -> &'static str {
        self.0
    }
}

impl std::fmt::Display for Op {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

/// Where in a call a rule acts: before the store is entered, or after it
/// answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Before,
    After,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Before => "before",
            Self::After => "after",
        })
    }
}

/// How a store error is classified, computed from the error itself so a law
/// cannot mislabel the fault it injects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// [`StoreError::is_transient`]: the same call may succeed when repeated.
    Transient,
    /// The store's deterministic refusal.
    Permanent,
    /// [`StoreError::StoredDataCorrupt`].
    Corrupt,
}

impl FaultKind {
    pub fn of(error: &StoreError) -> Self {
        if matches!(error, StoreError::StoredDataCorrupt { .. }) {
            Self::Corrupt
        } else if error.is_transient() {
            Self::Transient
        } else {
            Self::Permanent
        }
    }
}

/// What happened at one phase of one call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// No rule: the call entered the store.
    Entered,
    /// No rule: the store answered `Ok`.
    Returned,
    /// No rule: the store answered its own error.
    Refused,
    /// A rule answered the caller with an injected error. After the store
    /// answered, this is a lost reply: the store's work stands.
    Failed(FaultKind),
    /// A rule held the call at its gate.
    Paused,
    /// A rule stopped the call for good: it never returns.
    Crashed,
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Entered => "entered",
            Self::Returned => "returned",
            Self::Refused => "refused",
            Self::Failed(FaultKind::Transient) => "fail(transient)",
            Self::Failed(FaultKind::Permanent) => "fail(permanent)",
            Self::Failed(FaultKind::Corrupt) => "fail(corrupt)",
            Self::Paused => "pause",
            Self::Crashed => "crash",
        })
    }
}

/// One phase of one call through a [`Scripted`] store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Call {
    pub actor: Arc<str>,
    pub op: Op,
    /// This call's position among every actor's calls of `op`, from 1.
    pub nth: usize,
    /// This call's position among `actor`'s own calls of `op`, from 1.
    pub actor_nth: usize,
    pub phase: Phase,
    pub outcome: Outcome,
}

impl std::fmt::Display for Call {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}:{}#{} {} {}",
            self.actor, self.op, self.nth, self.phase, self.outcome
        )
    }
}

/// The error side of a listed operation. Each carries a store fault typed,
/// so an injected [`StoreError`] reaches the caller as the store would have
/// answered it.
pub trait ScriptedError {
    fn from_store_fault(error: StoreError) -> Self;
}

impl ScriptedError for StoreError {
    fn from_store_fault(error: StoreError) -> Self {
        error
    }
}

impl<R: Default> ScriptedError for MaintenanceFailure<R> {
    fn from_store_fault(error: StoreError) -> Self {
        Self::failed_before_any_work(error)
    }
}

impl ScriptedError for RootIntentRefused {
    fn from_store_fault(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone)]
enum Action {
    Fail(Arc<dyn Fn() -> StoreError + Send + Sync>, FaultKind),
    Pause(Arc<Gate>),
    Crash,
}

impl Action {
    fn outcome(&self) -> Outcome {
        match self {
            Self::Fail(_, kind) => Outcome::Failed(*kind),
            Self::Pause(_) => Outcome::Paused,
            Self::Crash => Outcome::Crashed,
        }
    }
}

#[derive(Clone, Copy)]
enum Hits {
    Nth(usize),
    From(usize),
}

/// The operations a rule acts on.
#[derive(Clone)]
enum Ops {
    One(Op),
    /// Each of these, or every operation when none is named.
    Each(Arc<[Op]>),
}

impl Ops {
    fn contains(&self, op: Op) -> bool {
        match self {
            Self::One(one) => *one == op,
            Self::Each(ops) => ops.is_empty() || ops.contains(&op),
        }
    }
}

impl std::fmt::Display for Ops {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::One(op) => write!(f, "{op}"),
            Self::Each(ops) if ops.is_empty() => f.write_str("*"),
            Self::Each(ops) => {
                let names: Vec<&str> = ops.iter().map(|op| op.name()).collect();
                write!(f, "{}", names.join("|"))
            }
        }
    }
}

/// The calls a rule acts on, and where in them.
#[derive(Clone)]
struct Target {
    ops: Ops,
    actor: Option<String>,
    hits: Hits,
    phase: Phase,
}

impl Target {
    fn matches(&self, call: &Call) -> bool {
        if !self.ops.contains(call.op) || self.phase != call.phase {
            return false;
        }
        let nth = match &self.actor {
            Some(actor) if **actor != *call.actor => return false,
            Some(_) => call.actor_nth,
            None => call.nth,
        };
        match self.hits {
            Hits::Nth(wanted) => nth == wanted,
            Hits::From(first) => nth >= first,
        }
    }
}

impl std::fmt::Display for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(actor) = &self.actor {
            write!(f, "{actor}:")?;
        }
        match self.hits {
            Hits::Nth(nth) => write!(f, "{}#{nth}", self.ops)?,
            Hits::From(first) => write!(f, "{}#{first}..", self.ops)?,
        }
        write!(f, " {}", self.phase)
    }
}

struct Rule {
    target: Target,
    action: Action,
    fired: usize,
}

impl std::fmt::Display for Rule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.target, self.action.outcome())
    }
}

#[derive(Default)]
struct State {
    rules: Vec<Rule>,
    trace: Vec<Call>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    recorded: tokio::sync::Notify,
}

impl Shared {
    /// Record one phase of a call and answer the first armed rule it fires.
    fn record(&self, mut call: Call, unruled: Outcome) -> Option<Action> {
        let action = {
            let mut state = self.state.lock_recover();
            let action = state
                .rules
                .iter_mut()
                .find(|rule| rule.target.matches(&call))
                .map(|rule| {
                    rule.fired += 1;
                    rule.action.clone()
                });
            call.outcome = action.as_ref().map_or(unruled, Action::outcome);
            state.trace.push(call);
            action
        };
        self.recorded.notify_waiters();
        action
    }

    fn calls(&self, op: Op) -> usize {
        Self::entries(&self.state.lock_recover().trace, op, None)
    }

    /// How often `op` was entered, by `actor` or by anyone.
    fn entries(trace: &[Call], op: Op, actor: Option<&str>) -> usize {
        trace
            .iter()
            .filter(|call| call.op == op && call.phase == Phase::Before)
            .filter(|call| actor.is_none_or(|actor| *call.actor == *actor))
            .count()
    }

    fn rendered_trace(&self) -> String {
        let state = self.state.lock_recover();
        if state.trace.is_empty() {
            return "trace: (no calls)".into();
        }
        let calls: Vec<String> = state.trace.iter().map(Call::to_string).collect();
        format!("trace: {}", calls.join(", "))
    }
}

/// The rules a law armed and the calls its stores made.
///
/// One script covers any number of stores and actors: [`Script::wrap`] hands
/// out a scripted store per actor, and all of them share the script's rules
/// and trace.
#[derive(Default)]
pub struct Script {
    shared: Arc<Shared>,
}

impl Script {
    pub fn new() -> Self {
        Self::default()
    }

    /// `inner` under this script. `actor` names the caller in the trace and
    /// in rules narrowed with [`On::by`].
    pub fn wrap<S: ?Sized>(&self, actor: &str, inner: Arc<S>) -> Arc<Scripted<S>> {
        Arc::new(Scripted {
            shared: Arc::clone(&self.shared),
            actor: Arc::from(actor),
            inner,
        })
    }

    /// Start a rule on `op`. It acts on the first call unless [`On::nth`] or
    /// [`On::from_nth`] says otherwise.
    pub fn on(&self, op: impl Into<Op>) -> On<'_> {
        On {
            script: self,
            target: Target {
                ops: Ops::One(op.into()),
                actor: None,
                hits: Hits::Nth(1),
                phase: Phase::Before,
            },
        }
    }

    /// Start a rule on every call of each of `ops`, or of every operation
    /// when `ops` is empty. [`On::nth`] and [`On::from_nth`] count the calls
    /// of each operation on its own.
    pub fn on_each(&self, ops: &[Op]) -> On<'_> {
        On {
            script: self,
            target: Target {
                ops: Ops::Each(ops.into()),
                actor: None,
                hits: Hits::From(1),
                phase: Phase::Before,
            },
        }
    }

    /// Every phase of every call so far, in the order they happened.
    pub fn trace(&self) -> Vec<Call> {
        self.shared.state.lock_recover().trace.clone()
    }

    /// How often `op` was called so far.
    pub fn calls(&self, op: impl Into<Op>) -> usize {
        self.shared.calls(op.into())
    }

    /// Wait until `op` was called `count` times. Panics with the trace when
    /// it has not been within [`GATE_DEADLINE`].
    pub async fn called(&self, op: impl Into<Op>, count: usize) {
        let op = op.into();
        let reached = async {
            loop {
                let recorded = self.shared.recorded.notified();
                if self.shared.calls(op) >= count {
                    return;
                }
                recorded.await;
            }
        };
        if tokio::time::timeout(GATE_DEADLINE, reached).await.is_err() {
            panic!(
                "`{op}` was called {} of {count} times within {GATE_DEADLINE:?}; {}",
                self.shared.calls(op),
                self.shared.rendered_trace()
            );
        }
    }

    fn arm(&self, target: Target, action: Action) {
        self.shared.state.lock_recover().rules.push(Rule {
            target,
            action,
            fired: 0,
        });
    }

    /// Arm a pause on `target` and answer its gate, which prints the rule
    /// and the trace when a wait on it expires.
    fn pause(&self, target: Target) -> Arc<Gate> {
        let shared: Weak<Shared> = Arc::downgrade(&self.shared);
        let gate = Arc::new(Gate::with_context(
            format!("{target} {}", Outcome::Paused),
            Box::new(move || {
                shared.upgrade().map_or_else(
                    || "the script was dropped".to_string(),
                    |shared| shared.rendered_trace(),
                )
            }),
        ));
        self.arm(target, Action::Pause(Arc::clone(&gate)));
        gate
    }
}

impl Drop for Script {
    /// Opens every gate, so nothing a failed law left held stays held, and
    /// fails the law when a rule it armed never fired.
    fn drop(&mut self) {
        let unfired: Vec<String> = {
            let state = self.shared.state.lock_recover();
            for rule in &state.rules {
                if let Action::Pause(gate) = &rule.action {
                    gate.open_all();
                }
            }
            state
                .rules
                .iter()
                .filter(|rule| rule.fired == 0)
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

/// A rule being armed: which calls of one operation it acts on.
pub struct On<'a> {
    script: &'a Script,
    target: Target,
}

impl<'a> On<'a> {
    /// Only `actor`'s calls, counted among its own.
    pub fn by(mut self, actor: &str) -> Self {
        self.target.actor = Some(actor.to_string());
        self
    }

    /// The `nth` call, from 1.
    pub fn nth(mut self, nth: usize) -> Self {
        assert!(nth > 0, "calls are counted from 1");
        self.target.hits = Hits::Nth(nth);
        self
    }

    /// The `first` call and every later one.
    pub fn from_nth(mut self, first: usize) -> Self {
        assert!(first > 0, "calls are counted from 1");
        self.target.hits = Hits::From(first);
        self
    }

    /// Act before the store is entered.
    pub fn before(mut self) -> Before<'a> {
        self.target.phase = Phase::Before;
        Before(self)
    }

    /// Act after the store answered: its work stands.
    pub fn after(mut self) -> After<'a> {
        self.target.phase = Phase::After;
        After(self)
    }
}

/// A rule that acts before the store is entered.
pub struct Before<'a>(On<'a>);

impl Before<'_> {
    /// Answer `fault()` instead of entering the store.
    pub fn fail(self, fault: impl Fn() -> StoreError + Send + Sync + 'static) {
        let kind = FaultKind::of(&fault());
        self.0
            .script
            .arm(self.0.target, Action::Fail(Arc::new(fault), kind));
    }

    /// Hold the call at the returned gate before it enters the store.
    pub fn pause(self) -> Arc<Gate> {
        self.0.script.pause(self.0.target)
    }

    /// Stop the call for good before it enters the store: it never returns.
    pub fn crash(self) {
        self.0.script.arm(self.0.target, Action::Crash);
    }
}

/// A rule that acts after the store answered.
pub struct After<'a>(On<'a>);

impl After<'_> {
    /// The store did its work, and the caller is answered a transient
    /// storage failure instead of the reply.
    pub fn lose_reply(self) {
        let lost = || StoreError::StorageFailure {
            backend: "script",
            message: "the reply was lost".to_string(),
        };
        let kind = FaultKind::of(&lost());
        self.0
            .script
            .arm(self.0.target, Action::Fail(Arc::new(lost), kind));
    }

    /// Hold the store's answer at the returned gate.
    pub fn pause(self) -> Arc<Gate> {
        self.0.script.pause(self.0.target)
    }

    /// The store did its work, and the call never returns.
    pub fn crash(self) {
        self.0.script.arm(self.0.target, Action::Crash);
    }
}

/// A store under a [`Script`]. It implements `RuntimeStoreDecorator`, and
/// `DeploymentStoreDecorator` over a deployment, with every listed operation
/// routed through [`Scripted::call`].
pub struct Scripted<S: ?Sized> {
    shared: Arc<Shared>,
    actor: Arc<str>,
    inner: Arc<S>,
}

impl<S: ?Sized> Scripted<S> {
    pub fn inner(&self) -> &S {
        self.inner.as_ref()
    }

    /// One call of `op`: apply the script before `store` runs and after it
    /// answered. The generated forwarders are the callers.
    pub async fn call<T, E: ScriptedError>(
        &self,
        op: impl Into<Op>,
        store: impl Future<Output = Result<T, E>>,
    ) -> Result<T, E> {
        let mut call = {
            let op = op.into();
            let state = self.shared.state.lock_recover();
            Call {
                nth: Shared::entries(&state.trace, op, None) + 1,
                actor_nth: Shared::entries(&state.trace, op, Some(&self.actor)) + 1,
                actor: Arc::clone(&self.actor),
                op,
                phase: Phase::Before,
                outcome: Outcome::Entered,
            }
        };
        if let Some(action) = self.shared.record(call.clone(), Outcome::Entered)
            && let Some(fault) = Self::apply(action).await
        {
            return Err(E::from_store_fault(fault));
        }
        let answer = store.await;
        call.phase = Phase::After;
        let unruled = if answer.is_ok() {
            Outcome::Returned
        } else {
            Outcome::Refused
        };
        if let Some(action) = self.shared.record(call, unruled)
            && let Some(fault) = Self::apply(action).await
        {
            return Err(E::from_store_fault(fault));
        }
        answer
    }

    /// Carry out a fired rule. `Some` is the fault the caller is answered.
    async fn apply(action: Action) -> Option<StoreError> {
        match action {
            Action::Fail(fault, _) => Some(fault()),
            Action::Pause(gate) => {
                gate.pass().await;
                None
            }
            Action::Crash => std::future::pending().await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{MaintenanceStop, StoreOp, VacuumReport};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    fn transient() -> StoreError {
        StoreError::Contended
    }

    fn permanent() -> StoreError {
        StoreError::UnsupportedStoreOperation { operation: "law" }
    }

    fn corrupt() -> StoreError {
        StoreError::StoredDataCorrupt {
            record_kind: "law",
            message: "unreadable".to_string(),
        }
    }

    /// A store whose every operation counts itself and answers its count.
    #[derive(Default)]
    struct Counter(AtomicUsize);

    impl Counter {
        async fn work(&self) -> Result<usize, StoreError> {
            Ok(self.0.fetch_add(1, Ordering::SeqCst) + 1)
        }
    }

    fn rendered(script: &Script) -> Vec<String> {
        script.trace().iter().map(Call::to_string).collect()
    }

    #[test]
    fn a_fault_is_classified_from_the_error() {
        assert_eq!(FaultKind::of(&transient()), FaultKind::Transient);
        assert_eq!(FaultKind::of(&permanent()), FaultKind::Permanent);
        assert_eq!(FaultKind::of(&corrupt()), FaultKind::Corrupt);
    }

    #[tokio::test]
    async fn a_fail_rule_answers_its_typed_fault_on_the_nth_call_without_entering_the_store() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        script
            .on(StoreOp::admit_root)
            .nth(2)
            .before()
            .fail(permanent);
        let call = || store.call(StoreOp::admit_root, store.inner().work());
        assert_eq!(call().await.expect("the first call passes"), 1);
        assert!(matches!(
            call().await,
            Err(StoreError::UnsupportedStoreOperation { operation: "law" })
        ));
        assert_eq!(call().await.expect("the third call passes"), 2);
        assert_eq!(
            rendered(&script),
            [
                "a:admit_root#1 before entered",
                "a:admit_root#1 after returned",
                "a:admit_root#2 before fail(permanent)",
                "a:admit_root#3 before entered",
                "a:admit_root#3 after returned",
            ]
        );
        assert_eq!(script.calls(StoreOp::admit_root), 3);
    }

    #[tokio::test]
    async fn a_lost_reply_keeps_the_stores_work_and_answers_a_transient_fault() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        script.on(StoreOp::admit_root).after().lose_reply();
        let lost = store
            .call(StoreOp::admit_root, store.inner().work())
            .await
            .expect_err("the reply is lost");
        assert!(matches!(lost, StoreError::StorageFailure { .. }));
        assert!(lost.is_transient());
        assert_eq!(
            store.inner().0.load(Ordering::SeqCst),
            1,
            "the store did its work"
        );
        assert_eq!(
            rendered(&script),
            [
                "a:admit_root#1 before entered",
                "a:admit_root#1 after fail(transient)",
            ]
        );
    }

    #[tokio::test]
    async fn a_sticky_rule_fails_every_call_from_its_first() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        script
            .on(StoreOp::lookup_session)
            .from_nth(2)
            .before()
            .fail(corrupt);
        let call = || store.call(StoreOp::lookup_session, store.inner().work());
        assert!(call().await.is_ok());
        for _ in 0..3 {
            assert!(matches!(
                call().await,
                Err(StoreError::StoredDataCorrupt { .. })
            ));
        }
        assert_eq!(store.inner().0.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_rule_narrowed_to_an_actor_counts_that_actors_calls() {
        let script = Script::new();
        let inner = Arc::new(Counter::default());
        let a = script.wrap("a", Arc::clone(&inner));
        let b = script.wrap("b", inner);
        script
            .on(StoreOp::load_turn_park)
            .by("b")
            .nth(1)
            .before()
            .fail(transient);
        for _ in 0..2 {
            assert!(
                a.call(StoreOp::load_turn_park, a.inner().work())
                    .await
                    .is_ok()
            );
        }
        assert!(matches!(
            b.call(StoreOp::load_turn_park, b.inner().work()).await,
            Err(StoreError::Contended)
        ));
        assert_eq!(
            rendered(&script).last().map(String::as_str),
            Some("b:load_turn_park#3 before fail(transient)")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_rule_on_each_operation_acts_on_every_call_of_the_named_ones() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        let held = script
            .on_each(&[StoreOp::load_turn_park.into(), StoreOp::admit_root.into()])
            .by("a")
            .before()
            .pause();
        let every = script.on_each(&[]).after().pause();
        every.open_all();
        store
            .call(StoreOp::lookup_session, store.inner().work())
            .await
            .expect("an operation the rule does not name passes");
        for (op, arrivals) in [(StoreOp::load_turn_park, 1), (StoreOp::admit_root, 2)] {
            let mut call = std::pin::pin!(store.call(op, store.inner().work()));
            held.reached_by(&mut call, arrivals).await;
            held.open_one();
            call.await.expect("the call passes once let through");
        }
        assert_eq!(
            every.arrived(),
            3,
            "a rule naming nothing acts on every operation"
        );
        assert_eq!(
            rendered(&script)[2..4],
            [
                "a:load_turn_park#1 before pause",
                "a:load_turn_park#1 after pause"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_holds_the_call_before_the_store_until_the_law_opens_its_gate() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        let held = script.on(StoreOp::record_turn_park).before().pause();
        let mut call = std::pin::pin!(store.call(StoreOp::record_turn_park, store.inner().work()));
        held.reached_by(&mut call, 1).await;
        assert_eq!(
            store.inner().0.load(Ordering::SeqCst),
            0,
            "a call held before the store has not entered it"
        );
        held.open_one();
        assert_eq!(call.await.expect("the call passes once let through"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_pause_after_the_store_holds_its_answer() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        let held = script.on(StoreOp::record_turn_park).after().pause();
        let mut call = std::pin::pin!(store.call(StoreOp::record_turn_park, store.inner().work()));
        held.reached_by(&mut call, 1).await;
        assert_eq!(store.inner().0.load(Ordering::SeqCst), 1);
        held.open_all();
        assert_eq!(call.await.expect("the held answer"), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_crashed_call_never_returns() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        script.on(StoreOp::commit_runtime_state).after().crash();
        let call = store.call(StoreOp::commit_runtime_state, store.inner().work());
        assert!(
            tokio::time::timeout(Duration::from_secs(60), call)
                .await
                .is_err()
        );
        assert_eq!(
            store.inner().0.load(Ordering::SeqCst),
            1,
            "the store committed before the crash"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_script_lets_every_held_call_through() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        let held = script.on(StoreOp::admit_root).before().pause();
        let mut call = std::pin::pin!(store.call(StoreOp::admit_root, store.inner().work()));
        held.reached_by(&mut call, 1).await;
        drop(script);
        assert_eq!(call.await.expect("the call passes"), 1);
    }

    #[tokio::test]
    #[should_panic(
        expected = "rule `admit_root#2 before fail(transient)`, `b:load_turn_park#1.. after pause` \
                    never fired; trace: a:admit_root#1 before entered, a:admit_root#1 after returned"
    )]
    async fn a_rule_that_never_fired_fails_the_law_with_the_trace() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        script
            .on(StoreOp::admit_root)
            .nth(2)
            .before()
            .fail(transient);
        let _held = script
            .on(StoreOp::load_turn_park)
            .by("b")
            .from_nth(1)
            .after()
            .pause();
        store
            .call(StoreOp::admit_root, store.inner().work())
            .await
            .expect("the first call passes");
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(
        expected = "`admit_root` was called 1 of 2 times within 10s; trace: a:admit_root#1 before entered"
    )]
    async fn waiting_for_a_call_that_never_comes_fails_at_the_deadline_with_the_trace() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(Counter::default()));
        store
            .call(StoreOp::admit_root, store.inner().work())
            .await
            .expect("the call passes");
        script.called(StoreOp::admit_root, 1).await;
        script.called(StoreOp::admit_root, 2).await;
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(
        expected = "gate `admit_root#1 before pause`: 0 of 1 arrivals within 10s; trace: (no calls)"
    )]
    async fn a_script_gate_nothing_reaches_fails_with_its_rule_and_the_trace() {
        let script = Script::new();
        let held = script.on(StoreOp::admit_root).before().pause();
        held.reached(1).await;
    }

    #[tokio::test]
    async fn a_fault_stays_typed_on_every_error_side_of_the_lists() {
        let script = Script::new();
        let store = script.wrap("a", Arc::new(()));
        script
            .on(StoreOp::vacuum)
            .before()
            .fail(|| StoreError::Contended);
        script
            .on(StoreOp::open_root_intent)
            .before()
            .fail(|| StoreError::Contended);
        let vacuum = store
            .call(StoreOp::vacuum, async {
                Ok::<VacuumReport, MaintenanceFailure<VacuumReport>>(VacuumReport::default())
            })
            .await
            .expect_err("the vacuum fails");
        assert!(matches!(
            vacuum.stop,
            MaintenanceStop::Failed(StoreError::Contended)
        ));
        assert_eq!(vacuum.partial, VacuumReport::default());
        let verb = store
            .call(StoreOp::open_root_intent, async {
                Ok::<(), RootIntentRefused>(())
            })
            .await
            .expect_err("the verb fails");
        assert!(matches!(
            verb,
            RootIntentRefused::Store(StoreError::Contended)
        ));
    }
}
