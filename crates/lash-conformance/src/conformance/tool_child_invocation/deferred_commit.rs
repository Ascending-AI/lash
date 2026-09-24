//! The deferred-commit law: a parked tool child's §4 point is its completion
//! resolution (ADR 0099 §4, §5; FIG-3609).
//!
//! A deferred leaf parks on its completion key; the resolution is its
//! terminal. Its final record commits there, after the after-tool hook
//! folds the resolved result and before the presentation boundary runs.
//! Two gates pin the child either side of that point, and a `Cancel` close
//! lands while the child is held:
//!
//! * **Held in presentation** — past the commit. The close decides nothing
//!   for the committed child: presentation finishes, and on a durable tier
//!   the reopen serves rank 0 as the child's success, not a cancellation.
//! * **Held in the after-tool hook** — resolved, not yet committed. The
//!   close's cancel decision takes the §4 point first: the close's token
//!   stops the held child, or its commit is refused with
//!   `RuntimeEffectGroupChildCancelDecided`. Either way no presentation runs
//!   beneath the decision, and on a durable tier rank 0 is the cancelled
//!   terminal the close decided.

use std::sync::atomic::{AtomicUsize, Ordering};

use pretty_assertions::assert_eq;

use super::*;

/// One hold point a law parks a child at: `entered` rises when the child
/// reaches it, the child waits for `released`, and `passed` counts the
/// times it went through.
struct HoldPoint {
    call_id: String,
    entered: tokio::sync::watch::Sender<bool>,
    released: tokio::sync::watch::Sender<bool>,
    passed: AtomicUsize,
}

impl HoldPoint {
    fn new(call_id: &str) -> Arc<Self> {
        Arc::new(Self {
            call_id: call_id.to_string(),
            entered: tokio::sync::watch::channel(false).0,
            released: tokio::sync::watch::channel(false).0,
            passed: AtomicUsize::new(0),
        })
    }

    async fn hold(&self) {
        self.entered.send_replace(true);
        let mut released = self.released.subscribe();
        let _ = released.wait_for(|released| *released).await;
        self.passed.fetch_add(1, Ordering::SeqCst);
    }

    async fn await_entered(&self, what: &str) {
        let mut entered = self.entered.subscribe();
        tokio::time::timeout(SETTLE_BUDGET, entered.wait_for(|entered| *entered))
            .await
            .unwrap_or_else(|_| panic!("the child never reached {what}"))
            .unwrap_or_else(|_| panic!("the {what} gate closed"));
    }

    fn release(&self) {
        self.released.send_replace(true);
    }

    async fn await_passed(&self, what: &str) {
        let deadline = std::time::Instant::now() + SETTLE_BUDGET;
        while self.passed.load(Ordering::SeqCst) == 0 {
            assert!(
                std::time::Instant::now() < deadline,
                "the child never went through {what}"
            );
            tokio::time::sleep(POLL).await;
        }
    }
}

/// The plugin factory the law's opener carries: a presentation step and an
/// after-tool hook, each parking the named call at its own hold point. The
/// hook holds only the resolved outcome — the parked attempt's own pending
/// outcome goes through untouched.
fn gated_factory(
    presentation: Arc<HoldPoint>,
    after_tool: Arc<HoldPoint>,
) -> Arc<dyn crate::plugin::PluginFactory> {
    let step: crate::plugin::ToolPresentationStep =
        Arc::new(move |input: crate::plugin::ToolPresentationInput| {
            let gate = Arc::clone(&presentation);
            let held = input.context.call_id == gate.call_id;
            let previous = input.previous;
            Box::pin(async move {
                if held {
                    gate.hold().await;
                }
                Ok::<_, crate::PluginError>(previous)
            })
        });
    let hook: crate::plugin::AfterToolCallHook =
        Arc::new(move |context: crate::plugin::ToolResultHookContext| {
            let gate = Arc::clone(&after_tool);
            let held = context.call_id == gate.call_id && context.result.as_done_output().is_some();
            Box::pin(async move {
                if held {
                    gate.hold().await;
                }
                Ok(Vec::new())
            })
        });
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "law-deferred-commit",
        crate::plugin::PluginSpec::new()
            .with_presentation_step(step)
            .with_after_tool_call(hook),
    ))
}

/// Which side of the §4 point the law holds the child on when it closes.
#[derive(Clone, Copy, Debug)]
enum HeldAt {
    /// After the commit, inside presentation.
    Presentation,
    /// Before the commit, inside the after-tool hook.
    AfterToolHook,
}

/// W6/W17 at the deferred boundary: a `Cancel` close between a deferred
/// child's resolution and its presentation leaves the committed child
/// protected, and a cancel decided before the commit still wins.
pub async fn a_deferred_childs_commit_point_is_its_resolution(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    for held_at in [HeldAt::Presentation, HeldAt::AfterToolHook] {
        deferred_close_while_held(fixture, &format!("{prefix}-{held_at:?}"), held_at).await;
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn deferred_close_while_held(fixture: &ToolChildLawFixture, prefix: &str, held_at: HeldAt) {
    let session_id = crate::SessionId::from(format!("{prefix}-deferred-commit"));
    let turn_id = crate::TurnId::from(format!("{prefix}-deferred-commit-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id);
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-deferred-commit-group");
    let call_id = format!("{group_key}-call-0");

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let host = world.host;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let presentation = HoldPoint::new(&call_id);
    let after_tool = HoldPoint::new(&call_id);
    let _guard = register_opener_with_extras(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&scenario.registry),
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
        OpenerExtras {
            plugin_factories: vec![gated_factory(
                Arc::clone(&presentation),
                Arc::clone(&after_tool),
            )],
            attachment_store: None,
        },
    );
    // Only the gate the law holds at stays shut; the other is open.
    match held_at {
        HeldAt::Presentation => after_tool.release(),
        HeldAt::AfterToolHook => presentation.release(),
    }
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let group = || async {
        single_leaf_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            LEAF_DEFERRED,
            deferrable_routing(fixture.deferrable_routing, &host),
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        )
    };
    let handle = scoped
        .controller()
        .open_effect_group(group().await)
        .await
        .expect("the group opens under the live opener");

    // The child parks, and its completion resolves.
    let key = scenario.observation.parked_key(&call_id).await;
    await_key_registered(&host, &session_id, &key).await;
    resolve_when_registered(
        &host,
        key,
        crate::Resolution::Ok(serde_json::json!({ "leaf": "deferred" })),
    )
    .await;

    // Held on one side of the §4 point, the caller closes under `Cancel`.
    let gate = match held_at {
        HeldAt::Presentation => &presentation,
        HeldAt::AfterToolHook => &after_tool,
    };
    gate.await_entered(&format!("{held_at:?}")).await;
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::Cancel)
        .await
        .expect("the caller closes under Cancel");
    gate.release();

    match held_at {
        HeldAt::Presentation => {
            // The committed child finished its presentation under the close.
            presentation.await_passed("presentation").await;
            assert_eq!(
                presentation.passed.load(Ordering::SeqCst),
                1,
                "the committed child's presentation ran once"
            );
        }
        HeldAt::AfterToolHook => {
            // The cancel decision took the §4 point first. Whether the
            // close's token stops the held child or its commit is refused
            // with `RuntimeEffectGroupChildCancelDecided`, nothing is
            // presented beneath the decision: given time to run,
            // presentation stays untouched.
            tokio::time::sleep(ABSENCE_BUDGET).await;
            assert!(
                !*presentation.entered.borrow(),
                "a child whose commit lost to the cancel decision presented nothing"
            );
        }
    }
    assert_eq!(
        scenario.observation.executions_of("law_deferred").len(),
        1,
        "the deferred body ran once"
    );

    if world.drain.is_none() {
        // A closed group's ranks are unreadable by contract on this tier; the
        // gates are the evidence.
        return;
    }
    // Rank 0 is the fact the §4 point decided. The close may return while
    // the child's task is still unwinding, so a same-process reopen is
    // retried until it is served.
    let deadline = std::time::Instant::now() + SETTLE_BUDGET;
    let settlement = loop {
        let mut handle = scoped
            .controller()
            .open_effect_group(group().await)
            .await
            .expect("the identical group reopens");
        match scoped
            .controller()
            .await_next_settlement(&mut handle, tokio_util::sync::CancellationToken::new())
            .await
        {
            Ok(settlement) => break settlement,
            Err(error) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "rank 0 was never served: {error}"
                );
                assert!(
                    error.to_string().contains("closed to its caller"),
                    "rank 0 failed to be served: {error}"
                );
                tokio::time::sleep(POLL).await;
            }
        }
    };
    assert_eq!(settlement.position, 0);
    match held_at {
        HeldAt::Presentation => {
            let Ok(crate::RuntimeEffectOutcome::ToolInvocation { outcome, .. }) =
                &settlement.outcome
            else {
                panic!("rank 0 is the committed child's tool invocation: {settlement:?}")
            };
            assert!(
                matches!(
                    outcome.record.output.outcome,
                    crate::ToolCallOutcome::Success(_)
                ),
                "rank 0 is the committed child's success, not the cancel the close asked \
                 for: {:?}",
                outcome.record.output
            );
        }
        HeldAt::AfterToolHook => {
            let error = settlement
                .outcome
                .as_ref()
                .err()
                .unwrap_or_else(|| panic!("rank 0 is the cancelled terminal: {settlement:?}"));
            assert_eq!(
                error.code.as_str(),
                "runtime_effect_group_child_cancelled",
                "the cancel decided before the commit is the recorded terminal"
            );
        }
    }
}
