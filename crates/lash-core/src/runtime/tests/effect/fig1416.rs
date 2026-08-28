//! Fail-closed defaults of the durable effect-group contract (FIG-1416).
//!
//! The commit that introduced the contract rested its whole behavioural claim on
//! "an out-of-tree controller that has not implemented groups fails closed with a
//! named error" — a claim carried in prose across a doc comment, an ADR, and a
//! release note, with nothing holding it. A defaulted trait method is exactly the
//! shape whose behaviour changes silently when someone gives it a working body,
//! so the default path is pinned here.

use super::*;

/// A controller that overrides nothing beyond the one required method, which is
/// what an out-of-tree host looks like the day groups land.
#[derive(Clone, Default)]
struct GrouplessEffectController {
    native: NativeRuntimeEffectController,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for GrouplessEffectController {}

#[async_trait::async_trait]
impl RuntimeEffectController for GrouplessEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.native.execute_effect(envelope, local_executor).await
    }
}

/// The other side of the coherence relation: a controller that declares support
/// and gives all three methods bodies.
///
/// Not a durable host — it journals nothing and settles children immediately —
/// but it is the *supporting* side of the invariant, and without it the relation
/// `supports_effect_groups() == !refuses` was only ever asserted at `false ==
/// !true`, which a flag hard-coded to `false` also satisfies.
#[derive(Default)]
struct GroupSupportingEffectController {
    native: NativeRuntimeEffectController,
    /// The disposition the open declared, so the close can resolve against it
    /// rather than accept whatever it is handed.
    declared: std::sync::Mutex<Option<crate::LoserPolicy>>,
}

#[async_trait::async_trait]
impl crate::AwaitEventResolver for GroupSupportingEffectController {}

#[async_trait::async_trait]
impl RuntimeEffectController for GroupSupportingEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.native.execute_effect(envelope, local_executor).await
    }

    fn supports_effect_groups(&self) -> bool {
        true
    }

    async fn open_effect_group(
        &self,
        group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, RuntimeEffectControllerError> {
        // Nothing to check about executors here: since FIG-1578 a group is
        // envelopes, and what runs a child is this host's registered resolver.
        *self.declared.lock().expect("declared disposition") = Some(group.loser_disposition());
        Ok(crate::EffectGroupHandle::new(&group))
    }

    async fn await_next_settlement(
        &self,
        handle: &mut crate::EffectGroupHandle,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> Result<crate::GroupSettlement, RuntimeEffectControllerError> {
        let position = handle.consumed();
        // The cursor of record advances on exactly the settlements returned, and
        // refuses rather than clamping if the caller awaited past the group.
        handle.advance()?;
        Ok(crate::GroupSettlement {
            position,
            sequence: position as u64 + 1,
            outcome: Ok(RuntimeEffectOutcome::Sleep),
        })
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        disposition: crate::LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let declared = self
            .declared
            .lock()
            .expect("declared disposition")
            .unwrap_or(disposition);
        crate::LoserPolicy::resolve_close(declared, disposition)?;
        Ok(())
    }
}

/// A resolver that has a runner for every envelope.
///
/// Enough for the coherence law, whose subject is the flag and the refusals
/// rather than what a child does: a settling child is all that is needed for
/// "this method is implemented" to be observable.
struct EveryChildRuns;

impl crate::GroupExecutors for EveryChildRuns {
    fn executor_for(
        &self,
        _envelope: &RuntimeEffectEnvelope,
    ) -> Option<crate::RuntimeEffectLocalExecutor<'static>> {
        Some(crate::RuntimeEffectLocalExecutor::testing(|_| async {
            Ok(RuntimeEffectOutcome::Sleep)
        }))
    }
}

fn one_child_group() -> crate::RuntimeEffectGroup {
    let child = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            crate::RuntimeScope::new("session"),
            "effect",
            RuntimeEffectKind::Sleep,
            "replay",
        ),
        RuntimeEffectCommand::Sleep { duration_ms: 1 },
    );
    crate::RuntimeEffectGroup::try_new(
        RuntimeInvocation::effect(
            crate::RuntimeScope::new("session"),
            "group",
            RuntimeEffectKind::Sleep,
            "group-replay",
        ),
        "session:group:batch:0",
        vec![child],
        crate::GroupWakePolicy::First,
        crate::LoserPolicy::Cancel,
    )
    .expect("a one-child group assembles")
}

#[tokio::test]
async fn a_controller_without_group_support_fails_closed_on_every_group_method() {
    let controller = GrouplessEffectController::default();
    assert!(
        !controller.supports_effect_groups(),
        "the capability flag must default to false, so deployment validation \
         refuses admission rather than discovering the gap mid-turn"
    );

    let group = one_child_group();
    let mut handle = crate::EffectGroupHandle::new(&group);
    let open = controller
        .open_effect_group(group)
        .await
        .expect_err("opening a group must fail closed");
    assert_eq!(
        open.code,
        crate::RuntimeErrorCode::EffectGroupUnsupported,
        "the refusal must be the named capability code, not a generic error"
    );

    let await_error = controller
        .await_next_settlement(&mut handle, tokio_util::sync::CancellationToken::new())
        .await
        .expect_err("awaiting a settlement must fail closed");
    assert_eq!(
        await_error.code,
        crate::RuntimeErrorCode::EffectGroupUnsupported
    );
    assert_eq!(
        handle.consumed(),
        0,
        "a failed await must not advance the cursor of record"
    );

    let close_error = controller
        .close_effect_group(handle, crate::LoserPolicy::Cancel)
        .await
        .expect_err("closing a group must fail closed");
    assert_eq!(
        close_error.code,
        crate::RuntimeErrorCode::EffectGroupUnsupported
    );
}

/// The coherence trap the capability flag creates: a host that overrides the
/// methods but forgets the flag reports "unsupported" while working, and one that
/// flips the flag but forgets a method fails at run time. Neither is detectable
/// from a single method, so the invariant is stated as a relation and asserted
/// against both sides of it.
#[tokio::test]
async fn the_group_capability_flag_and_the_group_methods_must_agree() {
    /// Asserted over all three methods rather than just `open`: the trap is a
    /// host that gives one method a body and leaves the others defaulted, which
    /// a single-method probe reads as coherent.
    async fn assert_coherent<C: RuntimeEffectController>(controller: &C) {
        let unsupported = crate::RuntimeErrorCode::EffectGroupUnsupported;
        let group = one_child_group();
        let declared = group.loser_disposition();
        let fallback = crate::EffectGroupHandle::new(&group);

        let opened = controller.open_effect_group(group).await;
        let refuses_open = opened
            .as_ref()
            .err()
            .is_some_and(|error| error.code == unsupported);
        let mut handle = opened.unwrap_or(fallback);

        let refuses_await = controller
            .await_next_settlement(&mut handle, tokio_util::sync::CancellationToken::new())
            .await
            .err()
            .is_some_and(|error| error.code == unsupported);
        let refuses_close = controller
            .close_effect_group(handle, declared)
            .await
            .err()
            .is_some_and(|error| error.code == unsupported);

        for (method, refuses) in [
            ("open_effect_group", refuses_open),
            ("await_next_settlement", refuses_await),
            ("close_effect_group", refuses_close),
        ] {
            assert_eq!(
                controller.supports_effect_groups(),
                !refuses,
                "supports_effect_groups() must answer true exactly when {method} is \
                 implemented; a host that flips one without the other either refuses \
                 work it can do or accepts work it cannot"
            );
        }
    }

    assert_coherent(&GrouplessEffectController::default()).await;
    assert_coherent(&GroupSupportingEffectController::default()).await;

    // The doubles above pin the relation's shape; the tier production actually
    // reaches is where the flag and the refusals can drift, and it has *two*
    // states because since FIG-1578 its answer is a per-deployment fact
    // established at wiring time rather than a constant. Both are asserted
    // against the same relation: unwired must refuse all three with the
    // capability code, wired must refuse none of them.
    let unwired = NativeRuntimeEffectController::default();
    assert!(
        !unwired.supports_effect_groups(),
        "a controller with no registered resolver has no runner for any child, \
         so deployment validation must be told before a group is ever opened"
    );
    assert_coherent(&unwired).await;

    let wired = NativeRuntimeEffectController::default();
    wired
        .register_group_executors(std::sync::Arc::new(EveryChildRuns))
        .expect("a fresh controller has no resolver yet");
    assert!(
        wired.supports_effect_groups(),
        "registering the resolver is what makes the native substrate support groups"
    );
    assert_coherent(&wired).await;
}

/// Two threads registering *different* resolvers at once: exactly one wins and
/// every loser is told, rather than silently discarded.
///
/// The refusal is what keeps "one host has one answer to what runs a child"
/// true, so it may not depend on timing. A `get`-then-`set` registration reads
/// `None` on both threads, writes on both, and hands the loser an `Ok` while its
/// resolver goes nowhere — a host that then routes children through a resolver
/// its wiring code believes is registered. `OnceLock::set` is therefore the
/// arbiter, and this pins that it is.
#[test]
fn concurrent_registration_of_different_resolvers_refuses_every_loser() {
    const REGISTRARS: usize = 8;

    for _ in 0..64 {
        let controller = NativeRuntimeEffectController::default();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(REGISTRARS));
        let outcomes = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..REGISTRARS)
                .map(|_| {
                    let controller = &controller;
                    let barrier = std::sync::Arc::clone(&barrier);
                    scope.spawn(move || {
                        // A distinct allocation per thread, so `Arc::ptr_eq`
                        // cannot mistake a loser for a re-registration.
                        let executors = std::sync::Arc::new(EveryChildRuns);
                        barrier.wait();
                        controller.register_group_executors(executors)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("a registrar thread"))
                .collect::<Vec<_>>()
        });

        let winners = outcomes.iter().filter(|outcome| outcome.is_ok()).count();
        assert_eq!(
            winners, 1,
            "exactly one of {REGISTRARS} different resolvers may be the host's \
             answer; a second Ok means a resolver was accepted and dropped"
        );
        for refusal in outcomes.into_iter().filter_map(Result::err) {
            assert_eq!(
                refusal.code,
                crate::RuntimeErrorCode::RuntimeEffectGroupShape,
                "a loser must learn its resolver is not the host's, with the \
                 typed refusal rather than a silent Ok"
            );
        }
        assert!(
            controller.supports_effect_groups(),
            "the winner's registration stands whatever the losers did"
        );
    }
}

/// A per-child routing miss on a *wired* host is a different fact from an
/// unwired host, and keeps its own code.
///
/// The two refusals answer different questions — "this deployment does not do
/// groups" versus "this deployment does groups but cannot route this child" —
/// and a caller that saw one code for both would have no way to tell a missing
/// wiring from a missing runner. The first is a deployment-validation failure;
/// the second names the child.
#[tokio::test]
async fn a_child_this_host_cannot_route_is_a_shape_refusal_not_an_unsupported_host() {
    struct NoChildRuns;

    impl crate::GroupExecutors for NoChildRuns {
        fn executor_for(
            &self,
            _envelope: &RuntimeEffectEnvelope,
        ) -> Option<crate::RuntimeEffectLocalExecutor<'static>> {
            None
        }
    }

    let controller = NativeRuntimeEffectController::default();
    controller
        .register_group_executors(std::sync::Arc::new(NoChildRuns))
        .expect("a fresh controller has no resolver yet");
    assert!(
        controller.supports_effect_groups(),
        "the flag is about the wiring, not about whether one particular child \
         happens to be routable"
    );

    let refusal = controller
        .open_effect_group(one_child_group())
        .await
        .expect_err("a child with no runner refuses the whole open");
    assert_eq!(
        refusal.code,
        crate::RuntimeErrorCode::RuntimeEffectGroupShape,
        "a routing miss is a shape refusal; reporting it as an unsupported host \
         would tell an operator to wire a resolver that is already wired"
    );
}
