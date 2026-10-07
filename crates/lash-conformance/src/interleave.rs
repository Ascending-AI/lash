//! A bounded interleaving explorer over store calls (ADR 0044 §Simulation).
//!
//! For laws whose actors are straight-line sequences of store calls that
//! meet only in the store. Every actor's store goes under one [`Script`],
//! which holds the actor before each call of the operations the law names.
//! The explorer lets one held call through at a time, and runs the law once
//! for every order of them, depth first. The law builds a fresh world for
//! each schedule and asserts an invariant after it:
//!
//! ```ignore
//! let mut explorer = Explorer::new("law").holding(&[StoreOp::load_session_head_meta.into()]);
//! while let Some(mut schedule) = explorer.next_schedule() {
//!     let world = World::new(schedule.index()).await;
//!     let a = schedule.actor("a", world.store());
//!     let b = schedule.actor("b", world.store());
//!     let answers = schedule
//!         .run(vec![("a", Box::pin(pass(&a))), ("b", Box::pin(pass(&b)))])
//!         .await;
//!     assert!(world.holds(&answers).await);
//! }
//! ```
//!
//! A call is held before it enters the store, so no actor waits inside a
//! store transaction and the explorer cannot deadlock on a write lock.
//!
//! The explorer refuses rather than misleads:
//! * an actor that neither reaches a held call nor finishes within
//!   [`GATE_DEADLINE`] waits on something the explorer does not control, and
//!   fails the law by name;
//! * an actor with two calls held at once is not one straight line;
//! * more than [`MAX_SCHEDULES`] schedules fail the law, so it cannot turn
//!   into a slow search;
//! * a law whose actors never met at a held call, or that names an operation
//!   no schedule reached, explored nothing and fails.
//!
//! A failed schedule is printed as `a:load_session_head_meta b:load_session_head_meta …`.
//! Setting [`REPLAY_VAR`] to that string runs that one schedule.
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures_util::future::{Either, join_all, select};
use lash_core::testing::{Call, GATE_DEADLINE, Gate, Op, Outcome, Phase, Script, Scripted};

/// The most schedules one law explores.
pub(crate) const MAX_SCHEDULES: usize = 1_000;

/// The environment variable that names the one schedule to run.
pub(crate) const REPLAY_VAR: &str = "LASH_INTERLEAVE";

/// One actor of a schedule: its whole run, as one future.
pub(crate) type Actor<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One step of the depth-first search: which of the actors offered there the
/// schedule lets through.
struct Choice {
    taken: usize,
    of: usize,
}

/// Every order of the held calls of one law's actors.
pub(crate) struct Explorer {
    law: &'static str,
    held: Vec<Op>,
    /// The choices the next schedule starts with.
    path: Vec<Choice>,
    explored: usize,
    /// The operations some schedule held an actor before.
    reached: BTreeSet<&'static str>,
    /// The one schedule to run, as `(actor, operation)` steps.
    replay: Option<Vec<(String, String)>>,
    exhausted: bool,
    concluded: bool,
}

impl Explorer {
    /// An explorer for `law` that holds every store call, or runs the one
    /// schedule [`REPLAY_VAR`] names.
    pub(crate) fn new(law: &'static str) -> Self {
        let replay = std::env::var(REPLAY_VAR)
            .ok()
            .filter(|schedule| !schedule.trim().is_empty());
        Self::over(law, replay.as_deref())
    }

    /// An explorer that runs the one schedule `schedule`, as a failed
    /// exploration printed it.
    #[cfg(test)]
    pub(crate) fn replaying(law: &'static str, schedule: &str) -> Self {
        Self::over(law, Some(schedule))
    }

    fn over(law: &'static str, replay: Option<&str>) -> Self {
        let replay = replay.map(|schedule| {
            schedule
                .split_whitespace()
                .map(|step| match step.split_once(':') {
                    Some((actor, op)) => (actor.to_string(), op.to_string()),
                    None => panic!(
                        "interleave `{law}`: `{step}` is no step of a schedule; a step is `actor:operation`"
                    ),
                })
                .collect()
        });
        Self {
            law,
            held: Vec::new(),
            path: Vec::new(),
            explored: 0,
            reached: BTreeSet::new(),
            replay,
            exhausted: false,
            concluded: false,
        }
    }

    /// Hold the actors only before `ops`. The law names the operations
    /// through which its actors can affect each other; the calls it leaves
    /// out run with the held call they follow.
    pub(crate) fn holding(mut self, ops: &[Op]) -> Self {
        self.held = ops.to_vec();
        self
    }

    /// The next schedule to run, or `None` once every one ran. The law runs
    /// each schedule it is handed.
    pub(crate) fn next_schedule(&mut self) -> Option<Schedule<'_>> {
        if self.exhausted {
            self.conclude();
            return None;
        }
        assert!(
            self.explored < MAX_SCHEDULES,
            "interleave `{}`: more than {MAX_SCHEDULES} schedules; hold fewer operations",
            self.law
        );
        Some(Schedule {
            script: Script::new(),
            seats: Vec::new(),
            steps: Vec::new(),
            ran: false,
            explorer: self,
        })
    }

    /// Record a finished schedule and move the search to the next one.
    fn finished(&mut self, steps: &[Step]) {
        self.explored += 1;
        self.reached.extend(steps.iter().map(|step| step.op.name()));
        if self.replay.is_some() {
            self.exhausted = true;
            return;
        }
        assert_eq!(
            steps.len(),
            self.path.len(),
            "interleave `{}`: the same choices ended after another number of steps; the actors \
             are not deterministic",
            self.law
        );
        while let Some(last) = self.path.last_mut() {
            if last.taken + 1 < last.of {
                last.taken += 1;
                return;
            }
            self.path.pop();
        }
        self.exhausted = true;
    }

    /// The vacuity guard, and the count the law prints.
    fn conclude(&mut self) {
        self.concluded = true;
        if self.replay.is_some() {
            println!("interleave `{}`: replayed one schedule", self.law);
            return;
        }
        assert!(
            self.explored > 1,
            "interleave `{}`: one schedule; its actors never met at a held call, so nothing was \
             interleaved",
            self.law
        );
        let unreached: Vec<&str> = self
            .held
            .iter()
            .map(|op| op.name())
            .filter(|op| !self.reached.contains(op))
            .collect();
        assert!(
            unreached.is_empty(),
            "interleave `{}`: no schedule reached `{}`; the law holds operations its actors \
             never call",
            self.law,
            unreached.join("`, `")
        );
        println!("interleave `{}`: {} schedules", self.law, self.explored);
    }
}

impl Drop for Explorer {
    /// A law that stops before the last schedule explored part of the orders.
    fn drop(&mut self) {
        if !self.concluded && !std::thread::panicking() {
            panic!(
                "interleave `{}`: the law stopped after {} schedules, before every order ran",
                self.law, self.explored
            );
        }
    }
}

/// One actor's place in a schedule.
struct Seat {
    name: String,
    /// Holds each of the actor's held calls.
    gate: Arc<Gate>,
    /// How many of its held calls the schedule let through.
    released: usize,
    /// How many steps it may take ahead of a waiting actor.
    yields_after: Option<usize>,
    /// How many steps it took ahead of a waiting actor.
    raced: usize,
}

#[derive(Clone, Copy)]
struct Step {
    actor: usize,
    op: Op,
}

/// One order of the actors' held calls, over one fresh world.
pub(crate) struct Schedule<'e> {
    explorer: &'e mut Explorer,
    script: Script,
    seats: Vec<Seat>,
    steps: Vec<Step>,
    ran: bool,
}

impl Schedule<'_> {
    /// This schedule's position in the exploration, from 0: a name for its
    /// world.
    pub(crate) fn index(&self) -> usize {
        self.explorer.explored
    }

    /// `inner` as actor `name` reaches it: under the schedule's script, held
    /// before each call the explorer holds. An actor may reach several
    /// stores; all of them are its one line of calls.
    pub(crate) fn actor<S: ?Sized>(&mut self, name: &str, inner: Arc<S>) -> Arc<Scripted<S>> {
        assert!(
            !name.is_empty() && !name.contains([':', ' ']),
            "an actor's name is one word of a schedule string"
        );
        if !self.seats.iter().any(|seat| seat.name == name) {
            let gate = self
                .script
                .on_each(&self.explorer.held)
                .by(name)
                .before()
                .pause();
            self.seats.push(Seat {
                name: name.to_string(),
                gate,
                released: 0,
                yields_after: None,
                raced: 0,
            });
        }
        self.script.wrap(name, inner)
    }

    /// Actor `name` repeats its calls until another actor's write lets it
    /// finish, as work an engine retries does: on its own it never ends.
    /// After `steps` steps taken ahead of a waiting actor, it waits for the
    /// others, which bounds the orders. When every waiting actor is past its
    /// bound, the one that took the fewest such steps goes next.
    pub(crate) fn yields_after(&mut self, name: &str, steps: usize) {
        match self.seats.iter_mut().find(|seat| seat.name == name) {
            Some(seat) => seat.yields_after = Some(steps),
            None => panic!("`{name}` is no actor of this schedule"),
        }
    }

    /// Every phase of every call the actors made, in the order they happened.
    pub(crate) fn trace(&self) -> Vec<Call> {
        self.script.trace()
    }

    /// Run the actors in this schedule's order and answer what each returned,
    /// in the order they were named. Afterwards nothing is held: the law
    /// reads its world through the same stores.
    pub(crate) async fn run<T>(&mut self, actors: Vec<(&str, Actor<'_, T>)>) -> Vec<T> {
        assert!(!self.ran, "a schedule runs its actors once");
        self.ran = true;
        let named: Vec<&str> = actors.iter().map(|(name, _)| *name).collect();
        let seated: Vec<&str> = self.seats.iter().map(|seat| seat.name.as_str()).collect();
        assert_eq!(
            named, seated,
            "a schedule runs one future per actor, in the order the actors were named"
        );
        let mut futures: Vec<Actor<'_, T>> = actors.into_iter().map(|(_, future)| future).collect();
        let mut outputs: Vec<Option<T>> = futures.iter().map(|_| None).collect();
        loop {
            self.settle(&mut futures, &mut outputs).await;
            let waiting: Vec<usize> = (0..outputs.len())
                .filter(|actor| outputs[*actor].is_none())
                .collect();
            if waiting.is_empty() {
                break;
            }
            let actor = self.choose(&waiting);
            let op = self.held_call(actor);
            let seat = &mut self.seats[actor];
            if waiting.len() > 1 {
                seat.raced += 1;
            }
            seat.released += 1;
            seat.gate.open_one();
            self.steps.push(Step { actor, op });
        }
        for seat in &self.seats {
            seat.gate.open_all();
        }
        self.explorer.finished(&self.steps);
        outputs.into_iter().flatten().collect()
    }

    /// Execute the actors until each is held before a call or finished.
    async fn settle<T>(&self, futures: &mut [Actor<'_, T>], outputs: &mut [Option<T>]) {
        let settled = join_all(
            futures
                .iter_mut()
                .zip(outputs.iter_mut())
                .zip(&self.seats)
                .map(|((future, output), seat)| async move {
                    if output.is_some() {
                        return;
                    }
                    let arrival = std::pin::pin!(seat.gate.arrivals(seat.released + 1));
                    if let Either::Left((finished, _)) = select(future.as_mut(), arrival).await {
                        *output = Some(finished);
                    }
                }),
        );
        let expired = tokio::time::timeout(GATE_DEADLINE, settled).await.is_err();
        for (actor, seat) in self.seats.iter().enumerate() {
            if outputs[actor].is_some() {
                continue;
            }
            let held = seat.gate.arrived() - seat.released;
            if expired && held == 0 {
                let after = self
                    .steps
                    .iter()
                    .rev()
                    .find(|step| step.actor == actor)
                    .map_or_else(
                        || "before its first held call".to_string(),
                        |step| format!("after `{}:{}`", seat.name, step.op),
                    );
                panic!(
                    "interleave `{}`: actor `{}` blocked outside the gates {after}: within \
                     {GATE_DEADLINE:?} it neither reached a held call nor finished, so it waits \
                     on something the explorer does not control",
                    self.explorer.law, seat.name
                );
            }
            assert!(
                held == 1,
                "interleave `{}`: actor `{}` has {held} calls held at once; its calls are not \
                 one straight line",
                self.explorer.law,
                seat.name
            );
        }
    }

    /// The call actor `actor` is held before.
    fn held_call(&self, actor: usize) -> Op {
        let name = self.seats[actor].name.as_str();
        let held = self.script.trace().into_iter().rev().find(|call| {
            *call.actor == *name && call.phase == Phase::Before && call.outcome == Outcome::Paused
        });
        match held {
            Some(call) => call.op,
            None => panic!("actor `{name}` is at its gate without a held call in the trace"),
        }
    }

    /// Which of the `waiting` actors this schedule lets through next.
    fn choose(&mut self, waiting: &[usize]) -> usize {
        let within_bound: Vec<usize> = waiting
            .iter()
            .copied()
            .filter(|actor| {
                let seat = &self.seats[*actor];
                seat.yields_after.is_none_or(|bound| seat.raced < bound)
            })
            .collect();
        // Once every waiting actor is past its bound, the one that raced
        // least goes, so two retried actors take turns.
        let offered = if within_bound.is_empty() {
            let least = waiting
                .iter()
                .copied()
                .min_by_key(|actor| self.seats[*actor].raced)
                .unwrap_or(waiting[0]);
            vec![least]
        } else {
            within_bound
        };
        let depth = self.steps.len();
        if let Some(replay) = &self.explorer.replay {
            let Some((name, op)) = replay.get(depth) else {
                return offered[0];
            };
            let named = waiting
                .iter()
                .copied()
                .find(|actor| self.seats[*actor].name == *name)
                .filter(|actor| self.held_call(*actor).name() == op);
            return match named {
                Some(actor) => actor,
                None => panic!(
                    "interleave `{}`: the schedule does not replay: step {} is `{name}:{op}`, \
                     and the actors are held before {}",
                    self.explorer.law,
                    depth + 1,
                    self.rendered_steps(waiting.iter().map(|actor| Step {
                        actor: *actor,
                        op: self.held_call(*actor),
                    }))
                ),
            };
        }
        let law = self.explorer.law;
        let path = &mut self.explorer.path;
        match path.get(depth) {
            Some(choice) => {
                assert_eq!(
                    choice.of,
                    offered.len(),
                    "interleave `{law}`: the same choices offered other actors at step {}; the \
                     actors are not deterministic",
                    depth + 1
                );
                offered[choice.taken]
            }
            None => {
                path.push(Choice {
                    taken: 0,
                    of: offered.len(),
                });
                offered[0]
            }
        }
    }

    /// The schedule so far, as [`REPLAY_VAR`] takes it.
    pub(crate) fn rendered(&self) -> String {
        self.rendered_steps(self.steps.iter().copied())
    }

    fn rendered_steps(&self, steps: impl Iterator<Item = Step>) -> String {
        let steps: Vec<String> = steps
            .map(|step| format!("{}:{}", self.seats[step.actor].name, step.op))
            .collect();
        steps.join(" ")
    }
}

impl Drop for Schedule<'_> {
    /// Names the schedule a failed law was in, and fails a law that was
    /// handed a schedule and did not run it.
    fn drop(&mut self) {
        if std::thread::panicking() {
            let schedule = self.rendered();
            eprintln!(
                "interleave `{}`: schedule {} failed after `{schedule}`\n\
                 replay it with {REPLAY_VAR}='{schedule}'",
                self.explorer.law,
                self.index() + usize::from(!self.ran),
            );
        } else if !self.ran {
            panic!(
                "interleave `{}`: the law dropped a schedule without running it",
                self.explorer.law
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StoreError, StoreOp};
    use std::sync::atomic::{AtomicUsize, Ordering};

    type Store = Arc<Scripted<AtomicUsize>>;

    const READ: StoreOp = StoreOp::load_session_head_meta;
    const WRITE: StoreOp = StoreOp::settle_observer_intents;

    /// The store's count, as `op` reads it.
    async fn read(store: &Store, op: StoreOp) -> usize {
        store
            .call(op, async {
                Ok::<_, StoreError>(store.inner().load(Ordering::SeqCst))
            })
            .await
            .unwrap_or_else(|error| panic!("the store reads: {error}"))
    }

    async fn write(store: &Store, value: usize) {
        store
            .call(WRITE, async {
                store.inner().store(value, Ordering::SeqCst);
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap_or_else(|error| panic!("the store writes: {error}"));
    }

    /// Read the count, then write one more: a lost update when two of these
    /// both read before either writes.
    async fn increment(store: &Store) {
        let seen = read(store, READ).await;
        write(store, seen + 1).await;
    }

    /// Explore two increments of one count and answer each schedule with the
    /// count it ended on.
    async fn two_increments(mut explorer: Explorer) -> Vec<(String, usize)> {
        let mut ended = Vec::new();
        while let Some(mut schedule) = explorer.next_schedule() {
            let count = Arc::new(AtomicUsize::new(0));
            let a = schedule.actor("a", Arc::clone(&count));
            let b = schedule.actor("b", Arc::clone(&count));
            schedule
                .run(vec![
                    ("a", Box::pin(increment(&a))),
                    ("b", Box::pin(increment(&b))),
                ])
                .await;
            ended.push((schedule.rendered(), count.load(Ordering::SeqCst)));
        }
        ended
    }

    #[tokio::test]
    async fn every_order_of_two_actors_calls_runs_once_and_the_racing_ones_lose_an_update() {
        let ended = two_increments(Explorer::over("increments", None)).await;
        let schedules: BTreeSet<&str> = ended.iter().map(|(schedule, _)| &**schedule).collect();
        assert_eq!(
            (ended.len(), schedules.len()),
            (6, 6),
            "two actors of two calls each interleave in six orders, each run once"
        );
        assert_eq!(
            ended[0].0,
            "a:load_session_head_meta a:settle_observer_intents b:load_session_head_meta b:settle_observer_intents"
        );
        for (schedule, count) in &ended {
            let lost = schedule.starts_with("a:load_session_head_meta b:load_session_head_meta")
                || schedule.starts_with("b:load_session_head_meta a:load_session_head_meta");
            assert_eq!(*count, if lost { 1 } else { 2 }, "{schedule}");
        }
    }

    #[tokio::test]
    async fn a_printed_schedule_replays_that_one_order() {
        let racing = "b:load_session_head_meta a:load_session_head_meta a:settle_observer_intents b:settle_observer_intents";
        let ended = two_increments(Explorer::replaying("increments", racing)).await;
        assert_eq!(ended, [(racing.to_string(), 1)]);
        // A prefix replays too: the steps after it run in the first order.
        let ended = two_increments(Explorer::replaying(
            "increments",
            "b:load_session_head_meta",
        ))
        .await;
        assert_eq!(
            ended,
            [(
                "b:load_session_head_meta a:load_session_head_meta a:settle_observer_intents b:settle_observer_intents"
                    .to_string(),
                1
            )]
        );
    }

    #[tokio::test]
    #[should_panic(
        expected = "the schedule does not replay: step 2 is `a:settle_observer_intents`, and the actors \
                    are held before a:load_session_head_meta b:settle_observer_intents"
    )]
    async fn a_schedule_the_actors_do_not_follow_fails_at_its_first_wrong_step() {
        two_increments(Explorer::replaying(
            "increments",
            "b:load_session_head_meta a:settle_observer_intents",
        ))
        .await;
    }

    #[tokio::test]
    async fn only_the_named_operations_are_held() {
        let ended =
            two_increments(Explorer::over("increments", None).holding(&[WRITE.into()])).await;
        let schedules: Vec<&str> = ended.iter().map(|(schedule, _)| &**schedule).collect();
        assert_eq!(
            schedules,
            [
                "a:settle_observer_intents b:settle_observer_intents",
                "b:settle_observer_intents a:settle_observer_intents"
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    #[should_panic(
        expected = "actor `b` blocked outside the gates after `b:load_session_head_meta`: within 10s it \
                    neither reached a held call nor finished"
    )]
    async fn an_actor_that_waits_outside_the_store_is_refused_by_name() {
        let mut explorer = Explorer::over("blocked", None);
        while let Some(mut schedule) = explorer.next_schedule() {
            let count = Arc::new(AtomicUsize::new(0));
            let a = schedule.actor("a", Arc::clone(&count));
            let b = schedule.actor("b", count);
            schedule
                .run(vec![
                    ("a", Box::pin(increment(&a))),
                    (
                        "b",
                        Box::pin(async {
                            read(&b, READ).await;
                            std::future::pending::<()>().await;
                        }),
                    ),
                ])
                .await;
        }
    }

    #[tokio::test]
    #[should_panic(expected = "actor `a` has 2 calls held at once")]
    async fn an_actor_with_two_calls_at_once_is_refused() {
        let mut explorer = Explorer::over("forked", None);
        while let Some(mut schedule) = explorer.next_schedule() {
            let a = schedule.actor("a", Arc::new(AtomicUsize::new(0)));
            schedule
                .run(vec![(
                    "a",
                    Box::pin(async {
                        tokio::join!(read(&a, READ), read(&a, READ));
                    }),
                )])
                .await;
        }
    }

    #[tokio::test]
    #[should_panic(expected = "interleave `wide`: more than 1000 schedules")]
    async fn an_exploration_past_the_cap_is_refused() {
        let mut explorer = Explorer::over("wide", None);
        while let Some(mut schedule) = explorer.next_schedule() {
            let count = Arc::new(AtomicUsize::new(0));
            let stores: Vec<Store> = ["a", "b", "c"]
                .into_iter()
                .map(|name| schedule.actor(name, Arc::clone(&count)))
                .collect();
            let four_reads = |store: &'_ Store| {
                let store = Arc::clone(store);
                async move {
                    for _ in 0..4 {
                        read(&store, READ).await;
                    }
                }
            };
            schedule
                .run(vec![
                    ("a", Box::pin(four_reads(&stores[0]))),
                    ("b", Box::pin(four_reads(&stores[1]))),
                    ("c", Box::pin(four_reads(&stores[2]))),
                ])
                .await;
        }
    }

    #[tokio::test]
    #[should_panic(expected = "one schedule; its actors never met at a held call")]
    async fn an_exploration_of_one_order_is_vacuous() {
        let mut explorer = Explorer::over("alone", None);
        while let Some(mut schedule) = explorer.next_schedule() {
            let a = schedule.actor("a", Arc::new(AtomicUsize::new(0)));
            schedule.run(vec![("a", Box::pin(increment(&a)))]).await;
        }
    }

    #[tokio::test]
    #[should_panic(expected = "no schedule reached `bind_run_inputs`")]
    async fn a_held_operation_no_actor_calls_fails_the_law() {
        two_increments(
            Explorer::over("stale", None).holding(&[WRITE.into(), StoreOp::bind_run_inputs.into()]),
        )
        .await;
    }

    #[tokio::test]
    #[should_panic(expected = "the law stopped after 1 schedules, before every order ran")]
    async fn a_law_that_stops_early_fails() {
        let mut explorer = Explorer::over("early", None);
        if let Some(mut schedule) = explorer.next_schedule() {
            let count = Arc::new(AtomicUsize::new(0));
            let a = schedule.actor("a", Arc::clone(&count));
            let b = schedule.actor("b", count);
            schedule
                .run(vec![
                    ("a", Box::pin(increment(&a))),
                    ("b", Box::pin(increment(&b))),
                ])
                .await;
        }
    }

    /// A retried actor reads until it sees the other's write. Unbounded, the
    /// order in which it is always let through never ends.
    #[tokio::test]
    async fn a_retried_actor_waits_for_the_others_after_its_bound() {
        let mut explorer = Explorer::over("retried", None);
        let mut schedules = Vec::new();
        while let Some(mut schedule) = explorer.next_schedule() {
            let count = Arc::new(AtomicUsize::new(0));
            let retried = schedule.actor("retried", Arc::clone(&count));
            let writer = schedule.actor("writer", count);
            schedule.yields_after("retried", 2);
            schedule
                .run(vec![
                    (
                        "retried",
                        Box::pin(async { while read(&retried, READ).await == 0 {} }),
                    ),
                    ("writer", Box::pin(write(&writer, 1))),
                ])
                .await;
            schedules.push(schedule.rendered());
        }
        assert_eq!(
            schedules,
            [
                "retried:load_session_head_meta retried:load_session_head_meta writer:settle_observer_intents \
                 retried:load_session_head_meta",
                "retried:load_session_head_meta writer:settle_observer_intents retried:load_session_head_meta",
                "writer:settle_observer_intents retried:load_session_head_meta",
            ]
        );
    }
}
