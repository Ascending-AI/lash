//! A group child whose lane is served by a deployment that cannot execute it
//! (FIG-4550), on a handler-driven engine.
//!
//! Two misses look alike from the child's invocation: its resolver has no
//! executor for it. A deployment that only does not carry the child now
//! leaves it accepted and its attempt retries; one whose wiring lacks what
//! the child needs can never run it, and retrying there is a loop its opener
//! waits on forever. Registered through `tool_child_unroutable_tests!`.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
/// * **Never.** A tool child whose opener is live nowhere, on a host with no
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
    install_child_host(&host, &process_env_store)
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
        let group = single_leaf_group(
            &scope,
            &session_id,
            &group_key,
            &env_ref,
            LEAF_RECOVERY,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &admitted).await,
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
