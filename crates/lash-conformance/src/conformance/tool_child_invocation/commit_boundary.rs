//! The commit-boundary laws: a group child's final commits at its attempt
//! boundary (ADR 0099 §4) and drains are admitted in recorded commit order
//! (§5), on the tier's real substrate — not on a test double.
//!
//! Both laws drive the `law_commit` leaf, whose body waits for a release the
//! law controls and whose settlement declares two recorded intents routed
//! through a [`GatedProcessService`]: the [`IntentSink`] parks an intent write
//! on command, which is exactly the observation "this child is past its §4
//! commit and inside its drain" — a fact no journal column exposes directly.

use pretty_assertions::assert_eq;

use super::*;

/// A group of `LEAF_COMMIT` children: child `position` carries call id
/// `{group_key}-call-{position}` under `disposition`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "the group's fields are the law's parameters; a struct would only rename the list"
)]
fn commit_group(
    scope: &crate::ExecutionScope,
    session_id: &crate::SessionId,
    group_key: &str,
    env_ref: &crate::ProcessExecutionEnvRef,
    children: usize,
    disposition: crate::LoserPolicy,
    routing: ToolChildCompletionRouting,
    cancellation: crate::TurnControlBindingId,
) -> crate::RuntimeEffectGroup {
    let parent = parent_invocation(scope);
    let children = (0..children)
        .map(|position| {
            child_envelope(
                scope,
                group_key,
                position,
                leaf_request(
                    scope,
                    session_id,
                    &format!("{group_key}-call-{position}"),
                    LEAF_COMMIT,
                    LEAF_COMMIT.trim_start_matches("tool:"),
                    catalog_admission(LEAF_COMMIT),
                    routing.clone(),
                    env_ref,
                    &parent,
                    cancellation.clone(),
                ),
            )
        })
        .collect();
    crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key,
        children,
        crate::GroupWakePolicy::All,
        disposition,
    )
    .expect("the commit group assembles")
}

/// Asserts `sink.landed()` stays empty for [`ABSENCE_BUDGET`]: no intent write
/// may land while a lower-commit sibling's drain is owed.
async fn assert_nothing_landed(sink: &IntentSink, context: &str) {
    tokio::time::sleep(ABSENCE_BUDGET).await;
    assert!(
        sink.landed().is_empty(),
        "{context}: an intent landed ahead of the committed-order barrier: {:?}",
        sink.landed()
    );
}

/// W6/W7: a child whose final record won the §4 point is protected — a close
/// under `Cancel` cannot interrupt its drain, and the drain it owes completes
/// with the committed settlement's intents exactly once.
///
/// * On a durable tier the crash window is real: the child commits, parks
///   inside its drain, the caller closes under `Cancel`, and the process dies.
///   The successor's drain replays the sealed drain input — the body never
///   runs again — and a reopen serves the success terminal at rank 0, not the
///   cancelled failure the close asked for.
/// * On a drain-less tier the same protection holds in-process: the close
///   decides no committed position, the child's held drain finishes once
///   released, and the intents land in declaration order. A closed group's
///   ranks are unreadable by contract on this tier, so the landed intents and
///   the single body execution are the success evidence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_childs_final_is_protected_and_its_drain_is_finished(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-protected"));
    let turn_id = crate::TurnId::from(format!("{prefix}-protected-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-protected-group");
    let call_0 = format!("{group_key}-call-0");

    let probe = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    {
        // The drain-less half: the close's cancel token must not interrupt the
        // committed child's drain.
        let host = probe.host;
        let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
        let sink = Arc::new(IntentSink::default());
        sink.hold_all();
        let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
            inner: crate::testing::effect_backed_process_service(
                Arc::clone(&scenario.registry),
                Arc::clone(&scenario.process_env_store),
            ),
            sink: Arc::clone(&sink),
        });
        let _guard = register_opener_with_processes(
            &host,
            &scope,
            Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
            processes,
            Arc::clone(&scenario.process_env_store),
            opener,
            tokio_util::sync::CancellationToken::new(),
        );
        let scoped = host
            .scoped(crate::admit(scope.clone()))
            .expect("the group scope binds");
        let handle = scoped
            .controller()
            .open_effect_group(commit_group(
                &scope,
                &session_id,
                &group_key,
                &scenario.env_ref,
                1,
                crate::LoserPolicy::Cancel,
                ToolChildCompletionRouting::Durable,
                recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
            ))
            .await
            .expect("the group opens under the live opener");
        // The child committed and is parked inside its drain.
        sink.await_blocked(&call_0).await;
        scoped
            .controller()
            .close_effect_group(handle, crate::LoserPolicy::Cancel)
            .await
            .expect("the caller closes under Cancel");
        // The close decided nothing for the committed child: its drain is
        // still held, nothing landed, and the body ran once.
        assert_nothing_landed(&sink, "a committed child closed under Cancel").await;
        assert_eq!(
            scenario.observation.executions_of("law_commit").len(),
            1,
            "the leaf body ran exactly once"
        );
        sink.release_all();
        sink.await_landed_len(2).await;
        assert_eq!(
            sink.landed(),
            vec![(call_0.clone(), "start"), (call_0.clone(), "event")],
            "the protected child's drain landed its declared intents in order"
        );
        assert_eq!(
            scenario.observation.executions_of("law_commit").len(),
            1,
            "the protected child's body never re-ran"
        );
    }
}

/// W19/W20: drains are admitted in recorded final-commit order, not source
/// order. Child B (source position 1) commits first and parks inside its
/// drain; child A (position 0) commits second and must hold at the §5 barrier
/// behind B — no intent of A's lands while B's drain is owed, and the
/// settlement ranks read B then A.
///
/// On a durable tier the crash half runs first: both children committed, B
/// parked mid-drain, A held at the barrier, and the process dies. The
/// successor re-drives both; the recovered drain lands B's intents then A's —
/// commit order, not whichever redrive reached its intents first — and the
/// bodies never re-run.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn drains_are_admitted_in_recorded_commit_order(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-commit-order"));
    let turn_id = crate::TurnId::from(format!("{prefix}-commit-order-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let call_a_of = |group_key: &str| format!("{group_key}-call-0");
    let call_b_of = |group_key: &str| format!("{group_key}-call-1");
    let expected_landed = |group_key: &str| {
        vec![
            (call_b_of(group_key), "start"),
            (call_b_of(group_key), "event"),
            (call_a_of(group_key), "start"),
            (call_a_of(group_key), "event"),
        ]
    };

    let probe = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;

    // The live half, on every tier: the same commit order holds while the
    // group is still open.
    let group_key = format!("{prefix}-commit-order");
    let call_a = call_a_of(&group_key);
    let call_b = call_b_of(&group_key);
    let host = probe.host;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let sink = Arc::new(IntentSink::default());
    scenario.observation.hold(&call_a);
    sink.hold(&call_b);
    let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
        inner: crate::testing::effect_backed_process_service(
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
        ),
        sink: Arc::clone(&sink),
    });
    let _guard = register_opener_with_processes(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        processes,
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
    );
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let mut handle = scoped
        .controller()
        .open_effect_group(commit_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            2,
            crate::LoserPolicy::RunToCompletion,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        ))
        .await
        .expect("the group opens under the live opener");
    // B commits first (commit_seq 1) and parks at its first intent write.
    sink.await_blocked(&call_b).await;
    // A's body is released; its attempt finishes and commits second — its
    // drain must wait at the §5 barrier behind B's owed drain.
    scenario.observation.release(&call_a);
    assert_nothing_landed(&sink, "A committed behind B's owed drain").await;
    sink.release(&call_b);
    sink.await_landed_len(4).await;
    assert_eq!(
        sink.landed(),
        expected_landed(&group_key),
        "the drains landed their intents in commit order, not source order"
    );
    let first = next_settlement(&scoped, &mut handle, 0).await;
    let second = next_settlement(&scoped, &mut handle, 1).await;
    assert_eq!(
        first.position, 1,
        "rank 0 is the child that committed first: B, at source position 1"
    );
    assert_eq!(
        second.position, 0,
        "rank 1 is the child that committed second: A, at source position 0"
    );
    assert!(first.outcome.is_ok() && second.outcome.is_ok());
    assert_eq!(
        scenario.observation.executions_of("law_commit").len(),
        2,
        "each leaf body ran exactly once"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");
}

/// How many times a drain held at the barrier may sleep the frozen dispatch
/// clock over [`ABSENCE_BUDGET`]. A wait on the host's own wake sleeps it not
/// at all; a poll on it, whose sleeps return at once, sleeps it without end.
const FROZEN_DISPATCH_SLEEP_BOUND: u64 = 16;

/// A dispatch clock whose faces never move and whose sleeps return at once,
/// counting every sleep: the clock a poll on the dispatch clock turns into a
/// hot loop, and a wait for its faces to move turns into a stall.
#[derive(Debug)]
struct FrozenDispatchClock {
    instant: std::time::Instant,
    sleeps: std::sync::atomic::AtomicU64,
}

impl FrozenDispatchClock {
    fn sleeps(&self) -> u64 {
        self.sleeps.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::Clock for FrozenDispatchClock {
    fn now(&self) -> std::time::Instant {
        self.instant
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        chrono::DateTime::from(std::time::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000))
    }

    async fn sleep(&self, _duration: Duration) {
        self.sleeps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    async fn sleep_until(&self, _deadline: std::time::Instant) {
        self.sleeps
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// FIG-3598: a drain held at the §5 barrier waits on its host's own wake for
/// the group, never on a poll of the dispatch clock. The live commit-order
/// scenario runs with its lent dispatch on a frozen clock: A holds at the
/// barrier behind B's owed drain without sleeping that clock — a poll on it,
/// whose sleeps return at once, would be a hot loop — and resumes as soon as
/// B's drain finishes, although the clock never moves.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_drain_held_at_the_barrier_parks_under_a_frozen_dispatch_clock(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-frozen-barrier"));
    let turn_id = crate::TurnId::from(format!("{prefix}-frozen-barrier-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id.clone());
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-frozen-barrier");
    let call_a = format!("{group_key}-call-0");
    let call_b = format!("{group_key}-call-1");

    let host = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await
    .host;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let sink = Arc::new(IntentSink::default());
    scenario.observation.hold(&call_a);
    sink.hold(&call_b);
    let processes: Arc<dyn crate::ProcessService> = Arc::new(GatedProcessService {
        inner: crate::testing::effect_backed_process_service(
            Arc::clone(&scenario.registry),
            Arc::clone(&scenario.process_env_store),
        ),
        sink: Arc::clone(&sink),
    });
    let clock = Arc::new(FrozenDispatchClock {
        instant: std::time::Instant::now(),
        sleeps: std::sync::atomic::AtomicU64::new(0),
    });
    let _guard = register_opener_inner(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Some(processes),
        None,
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras {
            clock: Some(Arc::clone(&clock) as Arc<dyn crate::Clock>),
            ..OpenerExtras::default()
        },
    );
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let handle = scoped
        .controller()
        .open_effect_group(commit_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            2,
            crate::LoserPolicy::RunToCompletion,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        ))
        .await
        .expect("the group opens under the live opener");
    // B commits first and parks at its first intent write; A's body is
    // released, and A commits second and holds at the barrier behind B.
    sink.await_blocked(&call_b).await;
    scenario.observation.release(&call_a);
    tokio::time::timeout(SETTLE_BUDGET, async {
        while scenario.observation.executions_of("law_commit").len() < 2 {
            tokio::time::sleep(POLL).await;
        }
    })
    .await
    .expect("A's leaf body ran once released");
    let sleeps_before = clock.sleeps();
    assert_nothing_landed(&sink, "A committed behind B's owed drain").await;
    let sleeps = clock.sleeps() - sleeps_before;
    assert!(
        sleeps <= FROZEN_DISPATCH_SLEEP_BOUND,
        "a drain held at the barrier must wait on its host's wake, not poll the dispatch \
         clock: it slept the frozen clock {sleeps} times in {ABSENCE_BUDGET:?}"
    );
    // B's drain finishing is what lifts the barrier; the frozen clock never
    // moves, so a wait that needed it to would stall here.
    sink.release(&call_b);
    sink.await_landed_len(4).await;
    assert_eq!(
        sink.landed(),
        vec![
            (call_b.clone(), "start"),
            (call_b, "event"),
            (call_a.clone(), "start"),
            (call_a, "event"),
        ],
        "the held drain resumed behind B's, in commit order"
    );
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");
}
