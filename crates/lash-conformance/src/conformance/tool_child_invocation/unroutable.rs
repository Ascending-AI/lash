//! A group child whose lane is served by a deployment that cannot execute it
//! (FIG-4550), on a handler-driven engine.
//!
//! Two misses look alike from the child's invocation: its resolver has no
//! executor for it. A deployment that only does not carry the child now
//! leaves it accepted and its attempt retries; one whose wiring lacks what
//! the child needs can never run it, and retrying there is a loop its opener
//! waits on forever. Which of the two a miss is, is the deployment's fact and
//! never one worker's: a child whose opener is live on another worker of the
//! same deployment is only misplaced (FIG-4590). Registered through
//! `tool_child_unroutable_tests!`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;

use super::*;

/// How long either half may take. The permanent half settles in a few engine
/// round trips; before FIG-4550 it never settled at all.
const ROUTE_BUDGET: Duration = Duration::from_secs(60);

/// The misses the uncarried child must take before a deployment carries it.
const UNCARRIED_ATTEMPTS: usize = 3;

/// A law resolver whose deployment routes its child and does not carry it
/// until told to: the default miss, which claims nothing permanent.
///
/// The open asks the resolver too, before the child exists; only a miss
/// taken once the group is open is an attempt of the child.
#[derive(Default)]
struct CarriedLater {
    carrying: AtomicBool,
    opened: Arc<AtomicBool>,
    misses: AtomicUsize,
}

impl crate::GroupExecutors for CarriedLater {
    fn executor_for(
        &self,
        envelope: &crate::RuntimeEffectEnvelope,
    ) -> Option<crate::RuntimeEffectLocalExecutor<'static>> {
        let crate::RuntimeEffectCommand::LanguageRuntimeValue { operation } = &envelope.command
        else {
            return None;
        };
        if !self.carrying.load(Ordering::SeqCst) {
            if self.opened.load(Ordering::SeqCst) {
                self.misses.fetch_add(1, Ordering::SeqCst);
            }
            return None;
        }
        let operation = operation.clone();
        Some(crate::RuntimeEffectLocalExecutor::testing(
            move |_| async move {
                Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue {
                    value: serde_json::json!({ "carried": operation }),
                })
            },
        ))
    }

    fn routes(&self, envelope: &crate::RuntimeEffectEnvelope) -> bool {
        matches!(
            envelope.command,
            crate::RuntimeEffectCommand::LanguageRuntimeValue { .. }
        )
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn uncarried_group(scope: &crate::ExecutionScope, group_key: &str) -> crate::RuntimeEffectGroup {
    let child = crate::RuntimeEffectEnvelope::new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:child:0"))
                .expect("valid group-child address"),
            crate::RuntimeAttribution::none(),
            "effect",
        ),
        crate::RuntimeEffectCommand::LanguageRuntimeValue {
            operation: "uncarried".to_owned(),
        },
    );
    crate::RuntimeEffectGroup::try_new(
        crate::RuntimeEffectInvocation::new(
            crate::EffectAddress::new(scope.clone(), format!("{group_key}:group"))
                .expect("valid group address"),
            crate::RuntimeAttribution::none(),
            "group",
        ),
        group_key,
        vec![child],
        crate::GroupWakePolicy::All,
        crate::LoserPolicy::RunToCompletion,
    )
    .expect("the single-child group assembles")
}

/// Opens `group` from the tier's handler, raises `opened`, and reports the
/// group's one settlement.
///
/// The attempt reports through a slot rather than a panic: a handler that
/// never sees the settlement is suspended, not failed, so the law bounds the
/// whole turn from outside it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
fn settle_one_child(
    group: crate::RuntimeEffectGroup,
    opened: Arc<AtomicBool>,
    settled: Arc<std::sync::Mutex<Option<crate::GroupSettlement>>>,
) -> crate::ConformanceTurnAttempt {
    Arc::new(move |scoped| {
        let group = group.clone();
        let opened = Arc::clone(&opened);
        let settled = Arc::clone(&settled);
        Box::pin(async move {
            let mut handle = scoped
                .controller()
                .open_effect_group(group)
                .await
                .expect("the endpoint routes the child, so the group opens");
            opened.store(true, Ordering::SeqCst);
            let settlement = scoped
                .controller()
                .await_next_settlement(
                    &mut handle,
                    lash_core::TurnCancelWait::unobserved(
                        tokio_util::sync::CancellationToken::new(),
                    ),
                )
                .await
                .expect("the group serves its one rank");
            scoped
                .controller()
                .close_effect_group(handle, crate::LoserPolicy::RunToCompletion)
                .await
                .expect("the settled group closes");
            *settled.lock_recover() = Some(settlement);
            crate::ConformanceTurnEnd::Settled
        })
    })
}

/// The route law: a child its deployment does not carry now keeps retrying
/// until one carries it, and a child its deployment can never execute settles
/// with the typed refusal naming the missing capability (FIG-4550).
///
/// * **Not carried.** The law's resolver routes its child and has no executor
///   for it. The child stays accepted across at least
///   [`UNCARRIED_ATTEMPTS`] attempts, holds no rank meanwhile, and settles
///   with its own outcome once the resolver carries it.
/// * **Never.** A tool child whose opener had no context to lend where it
///   formed the group, which the child records, on a deployment with no
///   tool-child context source: nothing on the deployment can build the
///   context it runs under, and the opener that could lend one is the caller
///   waiting on this child. It settles `Failed` with
///   `RuntimeEffectGroupChildUnroutable`, an outcome and never a live fault,
///   inside [`ROUTE_BUDGET`].
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_child_no_deployment_can_run_settles_typed_and_an_uncarried_one_retries(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let process_env_store = (fixture.make_processes)().await.process_env_store();
    let env_ref = crate::testing::process_execution_env_fixture(process_env_store.as_ref()).await;
    let host = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await
    .host;
    let later = Arc::new(CarriedLater::default());
    let child_host = install_child_host(&host, &process_env_store)
        .with_law_fallback(Arc::clone(&later) as Arc<dyn crate::GroupExecutors>);

    // Not carried: the child retries, and settles once it is carried.
    {
        let session_id = crate::SessionId::from(format!("{prefix}-uncarried"));
        let turn_id = crate::TurnId::from(format!("{prefix}-uncarried-turn"));
        let scope = crate::ExecutionScope::turn(session_id, turn_id);
        let group_key = format!("{prefix}-uncarried-group");
        let settled = Arc::new(std::sync::Mutex::new(None));
        let attempt = settle_one_child(
            uncarried_group(&scope, &group_key),
            Arc::clone(&later.opened),
            Arc::clone(&settled),
        );
        let carry = {
            let later = Arc::clone(&later);
            let settled = Arc::clone(&settled);
            async move {
                while later.misses.load(Ordering::SeqCst) < UNCARRIED_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    settled.lock_recover().is_none(),
                    "a child no deployment carries yet holds no rank"
                );
                later.carrying.store(true, Ordering::SeqCst);
            }
        };
        tokio::time::timeout(ROUTE_BUDGET, async {
            tokio::join!(
                fixture.turn_runner.run_turn(crate::admit(scope), attempt),
                carry
            )
        })
        .await
        .expect("the uncarried child settles once a deployment carries it");
        let settlement = settled
            .lock_recover()
            .take()
            .expect("the opener read its settlement");
        let Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue { value }) = &settlement.outcome
        else {
            panic!("the carried child settles its own outcome: {settlement:?}")
        };
        assert_eq!(value, &serde_json::json!({ "carried": "uncarried" }));
    }

    // Never: the child settles with the typed refusal instead of retrying.
    {
        let session_id = crate::SessionId::from(format!("{prefix}-unroutable"));
        let turn_id = crate::TurnId::from(format!("{prefix}-unroutable-turn"));
        let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id);
        let admitted = crate::admit(scope.clone());
        let group_key = format!("{prefix}-unroutable-group");
        // The opener forms its group as a session's does, and its own worker
        // answers what it lends: nothing, for it never registered.
        let opener_context = child_host.pin_open_tool_group(
            &group_key,
            &crate::EffectOpener::for_scope(&admitted).expect("a turn scope derives an opener"),
            [0],
        );
        assert_eq!(
            opener_context,
            crate::runtime::effect::ToolChildOpenerContext::Absent,
            "an opener that is not live where it forms its group lends no context"
        );
        let group = single_leaf_group_opened_with(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            LEAF_RECOVERY,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &admitted).await,
            opener_context,
        );
        let settled = Arc::new(std::sync::Mutex::new(None));
        tokio::time::timeout(
            ROUTE_BUDGET,
            fixture.turn_runner.run_turn(
                admitted,
                settle_one_child(group, Arc::default(), Arc::clone(&settled)),
            ),
        )
        .await
        .expect(
            "a child its deployment can never execute settles instead of retrying \
             `no executor currently routes` forever",
        );
        let settlement = settled
            .lock_recover()
            .take()
            .expect("the opener read its settlement");
        let Err(refusal) = &settlement.outcome else {
            panic!("an unroutable child never runs: {settlement:?}")
        };
        assert_eq!(
            refusal.code,
            crate::RuntimeErrorCode::RuntimeEffectGroupChildUnroutable,
            "{refusal}"
        );
        assert_eq!(
            refusal.cause,
            Some(crate::RuntimeErrorCause::EffectGroupChildUnroutable {
                missing: crate::GroupChildCapability::ToolChildContextSource,
            }),
            "the refusal names the missing capability: {refusal}"
        );
        assert_eq!(
            refusal.turn_failure_cause(),
            crate::TurnFailureCause::Outcome,
            "the refusal is the child's outcome, which no engine retries"
        );
    }
}

/// The other worker of the law's deployment: the one its opener is live on.
///
/// Installed behind the endpoint's own tool-child host, which is the worker
/// the child's invocation lands on. Until the law routes the child here,
/// every attempt is that worker's alone, and each one it could not serve is
/// counted; afterwards an attempt is served by this worker.
///
/// It answers for its own law's group only. A real server is shared and never
/// reset, and each law's process binds the same endpoint, so a child an
/// earlier law left retrying is delivered here too: it is missed uncounted,
/// or the count would say the law's own child was attempted (FIG-4623).
struct OpenerWorker {
    group_key: String,
    worker: Arc<crate::runtime::effect::ToolChildHost>,
    routed_here: AtomicBool,
    misses_elsewhere: AtomicUsize,
}

impl crate::GroupExecutors for OpenerWorker {
    fn executor_for(
        &self,
        envelope: &crate::RuntimeEffectEnvelope,
    ) -> Option<crate::RuntimeEffectLocalExecutor<'static>> {
        if !matches!(
            envelope.command,
            crate::RuntimeEffectCommand::ToolInvocation { .. }
        ) {
            return None;
        }
        if (envelope.group.as_deref()).is_none_or(|group| group.group_key != self.group_key) {
            return None;
        }
        if !self.routed_here.load(Ordering::SeqCst) {
            self.misses_elsewhere.fetch_add(1, Ordering::SeqCst);
            return None;
        }
        crate::GroupExecutors::executor_for(self.worker.as_ref(), envelope)
    }

    fn routes(&self, _envelope: &crate::RuntimeEffectEnvelope) -> bool {
        false
    }
}

/// The placement law: a child that lands on a worker its opener is not live
/// on, while the opener is live on another worker of the same deployment, is
/// misplaced and not unroutable (FIG-4590).
///
/// One deployment, two workers, and no tool-child context source on either.
/// The opener is live on worker A and forms its group there, so its child
/// records a lent context. The child's invocation lands on worker B, which
/// holds no opener, no pin and no source for it. B's lack is where the child
/// was placed: the child stays accepted across at least
/// [`UNCARRIED_ATTEMPTS`] attempts on B, holds no rank meanwhile, and runs
/// once, to its own outcome, when an attempt is routed to A.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_child_whose_opener_is_live_on_another_worker_retries_until_routed_there(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-placed"));
    let turn_id = crate::TurnId::from(format!("{prefix}-placed-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id);
    let admitted = crate::admit(scope.clone());
    let opener = crate::EffectOpener::for_scope(&admitted).expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-placed-group");
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let host = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await
    .host;

    // Worker A: its own tool-child host over the deployment's substrate,
    // with the opener live on it for the whole law.
    let worker_a = crate::runtime::effect::ToolChildHost::new(
        &host,
        Arc::clone(&scenario.process_env_store),
        Arc::new(crate::facade_support::SystemClock),
    );
    let _live_on_a = register_opener_on(
        &worker_a,
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        None,
        Some(Arc::clone(&scenario.registry)),
        Arc::clone(&scenario.process_env_store),
        opener.clone(),
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras::default(),
    );
    // Worker B: the endpoint's installed host, where the child's invocation
    // lands. The opener never registers on it.
    let placement = Arc::new(OpenerWorker {
        group_key: group_key.clone(),
        worker: Arc::clone(&worker_a),
        routed_here: AtomicBool::new(false),
        misses_elsewhere: AtomicUsize::new(0),
    });
    install_child_host(&host, &scenario.process_env_store)
        .with_law_fallback(Arc::clone(&placement) as Arc<dyn crate::GroupExecutors>);

    // The opener forms its group on A, whose answer the child records.
    let opener_context = worker_a.pin_open_tool_group(&group_key, &opener, [0]);
    assert_eq!(
        opener_context,
        crate::runtime::effect::ToolChildOpenerContext::Lent,
        "an opener live where it forms its group lends its context"
    );
    let group = single_leaf_group_opened_with(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        LEAF_PLAIN,
        ToolChildCompletionRouting::Inline,
        recorded_cancellation_authority(&host, &admitted).await,
        opener_context,
    );

    let settled = Arc::new(std::sync::Mutex::new(None));
    let attempt = settle_one_child(group, Arc::default(), Arc::clone(&settled));
    let route_to_a = {
        let placement = Arc::clone(&placement);
        let settled = Arc::clone(&settled);
        async move {
            while placement.misses_elsewhere.load(Ordering::SeqCst) < UNCARRIED_ATTEMPTS {
                assert!(
                    settled.lock_recover().is_none(),
                    "a child whose opener is live on another worker of its deployment is \
                     misplaced, and one worker's miss settled it: {:?}",
                    settled.lock_recover()
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            assert!(
                settled.lock_recover().is_none(),
                "a child no attempt has run yet holds no rank"
            );
            placement.routed_here.store(true, Ordering::SeqCst);
        }
    };
    tokio::time::timeout(ROUTE_BUDGET, async {
        tokio::join!(fixture.turn_runner.run_turn(admitted, attempt), route_to_a)
    })
    .await
    .expect("the misplaced child settles once an attempt is routed to its opener's worker");

    let settlement = settled
        .lock_recover()
        .take()
        .expect("the opener read its settlement");
    let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) = &settlement.outcome
    else {
        panic!("the child runs on its opener's worker and settles its own outcome: {settlement:?}")
    };
    assert!(
        format!("{:?}", outcome.record.output).contains("plain"),
        "the settled output is the leaf's own: {outcome:?}"
    );
    assert_eq!(
        scenario.observation.executions_of("law_plain").len(),
        1,
        "the leaf body ran exactly once, on the worker its opener is live on"
    );
}

/// A lost lending worker does not end its durable opener (FIG-4604). With no
/// context source, its child retries until the opener recovers or durably
/// ends. The production end closes its outstanding groups before committing
/// the terminal, and the index seats the uncommitted child as cancelled
/// without needing an executor. No permanent routing verdict is needed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_lent_child_is_cancelled_by_its_openers_durable_end(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-lent-end"));
    let scope = crate::ExecutionScope::turn(
        session_id.clone(),
        crate::TurnId::from(format!("{prefix}-lent-end-turn")),
    );
    let admitted = crate::admit(scope.clone());
    let opener = crate::EffectOpener::for_scope(&admitted).expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-lent-end-group");
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let host = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await
    .host;
    let lender = crate::runtime::effect::ToolChildHost::new(
        &host,
        Arc::clone(&scenario.process_env_store),
        Arc::new(crate::facade_support::SystemClock),
    );
    let live = register_opener_on(
        &lender,
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        None,
        Some(Arc::clone(&scenario.registry)),
        Arc::clone(&scenario.process_env_store),
        opener.clone(),
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras::default(),
    );
    let opener_context = lender.pin_open_tool_group(&group_key, &opener, [0]);
    assert_eq!(
        opener_context,
        crate::runtime::effect::ToolChildOpenerContext::Lent
    );
    let group = single_leaf_group_opened_with(
        &scope,
        &session_id,
        &group_key,
        &scenario.env_ref,
        LEAF_PLAIN,
        ToolChildCompletionRouting::Inline,
        recorded_cancellation_authority(&host, &admitted).await,
        opener_context,
    );
    let placement = Arc::new(OpenerWorker {
        group_key: group_key.clone(),
        worker: lender,
        routed_here: AtomicBool::new(false),
        misses_elsewhere: AtomicUsize::new(0),
    });
    install_child_host(&host, &scenario.process_env_store)
        .with_law_fallback(Arc::clone(&placement) as Arc<dyn crate::GroupExecutors>);
    // Lose the worker's registration after lending, with no pin or source
    // on the surviving worker. This does not close the durable opener.
    drop(live);

    let attempt: crate::ConformanceTurnAttempt = {
        let host = Arc::clone(&host);
        let process_env_store = Arc::clone(&scenario.process_env_store);
        let session_id = session_id.clone();
        let group_key = group_key.clone();
        let placement = Arc::clone(&placement);
        Arc::new(move |scoped| {
            let host = Arc::clone(&host);
            let process_env_store = Arc::clone(&process_env_store);
            let session_id = session_id.clone();
            let group = group.clone();
            let group_key = group_key.clone();
            let placement = Arc::clone(&placement);
            Box::pin(async move {
                let handle = scoped
                    .controller()
                    .open_effect_group(group)
                    .await
                    .expect("the endpoint serves the recorded child");
                while placement.misses_elsewhere.load(Ordering::SeqCst) < UNCARRIED_ATTEMPTS {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    scoped
                        .controller()
                        .read_group_settlement(&group_key, 1)
                        .await
                        .expect("the live opener's rank is readable")
                        .is_none(),
                    "worker loss leaves the non-terminal opener's child accepted"
                );
                let context = crate::testing::TestExecutionContextBuilder::new(
                    crate::testing::TestExecutionPorts::over_host(host, process_env_store),
                )
                .session_id(session_id)
                .borrowed_effect_controller(scoped)
                .build()
                .into_runtime()
                .with_opener_state(crate::session::OpenerState::default());
                context
                    .restore_outstanding_groups(vec![handle], &std::collections::BTreeMap::new());
                let closed = context
                    .close_opener_groups()
                    .await
                    .expect("the production opener end needs no executor for an uncommitted child");
                assert_eq!(closed.groups, vec![group_key]);
                assert!(closed.pending.is_empty());
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    tokio::time::timeout(
        ROUTE_BUDGET,
        fixture.turn_runner.run_turn(admitted.clone(), attempt),
    )
    .await
    .expect("the opener ends without waiting for its lost lending worker");

    let scoped = host
        .scoped(admitted)
        .expect("the terminal opener's scope binds");
    let rank = scoped
        .controller()
        .read_group_settlement(&group_key, 1)
        .await
        .expect("the terminal opener's durable rank is readable")
        .expect("the opener's end seated its child before committing its terminal");
    assert_eq!(rank.sequence, 1);
    let refusal = rank
        .outcome
        .expect_err("the uncommitted child is cancelled by its opener's end");
    assert_eq!(
        refusal.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
        "the durable end cancels, rather than fabricating a permanent routing refusal"
    );
    let attempts_at_end = placement.misses_elsewhere.load(Ordering::SeqCst);
    tokio::time::sleep(ABSENCE_BUDGET).await;
    let attempts_after_end = placement.misses_elsewhere.load(Ordering::SeqCst);
    assert!(
        attempts_after_end <= attempts_at_end + 1,
        "at most one final delivery observes the durable cancel, then the child stops; \
         attempts grew from {attempts_at_end} to {attempts_after_end}"
    );
    assert!(
        scenario.observation.executions_of("law_plain").is_empty(),
        "neither worker ran the child"
    );
}
