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
struct GrouplessEffectController;

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for GrouplessEffectController {
    /// An out-of-tree host that names no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for GrouplessEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        local_executor.execute(envelope).await
    }

    async fn open_effect_group(
        &self,
        _group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        // The refusing side of the typed-refusal-from-wiring law: this double
        // runs ordinary effects locally and deliberately implements no groups, which is now something its source states
        // rather than something it inherited.
        Err(lash_core::effect_groups_unsupported(
            "GrouplessEffectController",
        ))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "GrouplessEffectController",
        ))
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        _disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        Err(lash_core::effect_groups_unsupported(
            "GrouplessEffectController",
        ))
    }
}

/// The other side of the coherence relation: a controller that declares support
/// and gives all three methods bodies.
///
/// Not a durable host — it journals nothing and settles children immediately —
/// but it is the *supporting* side of the invariant, and without it the
/// one-surface law would only ever be asserted against controllers that refuse,
/// which a blanket refusal also satisfies.
#[derive(Default)]
struct GroupSupportingEffectController {
    /// The disposition the open declared, so the close can resolve against it
    /// rather than accept whatever it is handed.
    declared: std::sync::Mutex<Option<lash_core::LoserPolicy>>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for GroupSupportingEffectController {
    /// An out-of-tree host that names no durable authority.
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for GroupSupportingEffectController {
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: lash_core::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        local_executor.execute(envelope).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, RuntimeEffectControllerError> {
        // Nothing to check about executors here: since FIG-1578 a group is
        // envelopes, and what runs a child is this host's registered resolver.
        *self.declared.lock().expect("declared disposition") = Some(group.loser_disposition());
        Ok(lash_core::EffectGroupHandle::new(&group))
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        _cancel: lash_core::TurnCancelWait,
    ) -> Result<lash_core::GroupSettlement, RuntimeEffectControllerError> {
        let position = handle.consumed();
        // The cursor of record advances on exactly the settlements returned, and
        // refuses rather than clamping if the caller awaited past the group.
        handle.advance()?;
        Ok(lash_core::GroupSettlement {
            position,
            sequence: position as u64 + 1,
            outcome: Ok(RuntimeEffectOutcome::Sleep),
        })
    }

    async fn close_effect_group(
        &self,
        _handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        let declared = self
            .declared
            .lock()
            .expect("declared disposition")
            .unwrap_or(disposition);
        lash_core::LoserPolicy::resolve_close(declared, disposition)?;
        Ok(())
    }
}

fn one_child_group() -> lash_core::RuntimeEffectGroup {
    let scope = lash_core::ExecutionScope::turn("session", "turn");
    let child = RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope.clone(), "replay")
                .expect("valid group child address"),
            lash_core::RuntimeAttribution::for_session("session"),
            "effect",
        ),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 1 },
        },
    );
    lash_core::RuntimeEffectGroup::try_new(
        RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scope, "group-replay").expect("valid group address"),
            lash_core::RuntimeAttribution::for_session("session"),
            "group",
        ),
        "session:group:batch:0",
        vec![child],
        lash_core::GroupWakePolicy::First,
        lash_core::LoserPolicy::Cancel,
    )
    .expect("a one-child group assembles")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_controller_without_group_support_fails_closed_on_every_group_method() {
    let controller = GrouplessEffectController;

    let group = one_child_group();
    let mut handle = lash_core::EffectGroupHandle::new(&group);
    let open = controller
        .open_effect_group(group)
        .await
        .expect_err("opening a group must fail closed");
    assert_eq!(
        open.code,
        lash_core::RuntimeErrorCode::EffectGroupUnsupported,
        "the refusal must be the named capability code, not a generic error"
    );

    let await_error = controller
        .await_next_settlement(
            &mut handle,
            lash_core::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new()),
        )
        .await
        .expect_err("awaiting a settlement must fail closed");
    assert_eq!(
        await_error.code,
        lash_core::RuntimeErrorCode::EffectGroupUnsupported
    );
    assert_eq!(
        handle.consumed(),
        0,
        "a failed await must not advance the cursor of record"
    );

    let close_error = controller
        .close_effect_group(handle, lash_core::LoserPolicy::Cancel)
        .await
        .expect_err("closing a group must fail closed");
    assert_eq!(
        close_error.code,
        lash_core::RuntimeErrorCode::EffectGroupUnsupported
    );
}

/// All three group methods are one surface: a controller implements them
/// together or refuses them together.
///
/// This replaces the `supports_effect_groups()` coherence relation FIG-2266
/// deleted. That relation compared a boolean against the methods, which caught
/// a host that flipped one without the other — but it also made "I have not
/// thought about groups" and "I refuse groups" the same program text, because
/// the flag defaulted to `false` beside three methods that defaulted to
/// refusing. A delegating wrapper that forgot to forward therefore looked
/// *coherent* while denying a capability its inner controller had.
///
/// With no defaults left, the surviving invariant is stronger and needs no
/// flag: whatever a controller answers, it answers with all three. A wrapper
/// that forwards `open_effect_group` and leaves the other two refusing is the
/// same bug the old relation existed to catch, and this law still catches it.
#[tokio::test(flavor = "multi_thread")]
async fn the_three_group_methods_answer_as_one_surface() {
    /// Which of the three refused with the capability code.
    async fn refusals<C: RuntimeEffectController>(controller: &C) -> [(&'static str, bool); 3] {
        let unsupported = lash_core::RuntimeErrorCode::EffectGroupUnsupported;
        let group = one_child_group();
        let declared = group.loser_disposition();
        let fallback = lash_core::EffectGroupHandle::new(&group);

        let opened = controller.open_effect_group(group).await;
        let refuses_open = opened
            .as_ref()
            .err()
            .is_some_and(|error| error.code == unsupported);
        let mut handle = opened.unwrap_or(fallback);

        let refuses_await = controller
            .await_next_settlement(
                &mut handle,
                lash_core::TurnCancelWait::unobserved(tokio_util::sync::CancellationToken::new()),
            )
            .await
            .err()
            .is_some_and(|error| error.code == unsupported);
        let refuses_close = controller
            .close_effect_group(handle, declared)
            .await
            .err()
            .is_some_and(|error| error.code == unsupported);
        [
            ("open_effect_group", refuses_open),
            ("await_next_settlement", refuses_await),
            ("close_effect_group", refuses_close),
        ]
    }

    async fn assert_all<C: RuntimeEffectController>(controller: &C, expected: bool, what: &str) {
        for (method, refuses) in refusals(controller).await {
            assert_eq!(
                refuses,
                expected,
                "{what}: {method} must {} the capability code like its two \
                 siblings; a controller that answers one way through one method \
                 and the other way through another has no single answer to \
                 whether this deployment does groups",
                if expected {
                    "refuse with"
                } else {
                    "not refuse with"
                }
            );
        }
    }

    assert_all(
        &GrouplessEffectController,
        true,
        "a controller that implements no groups",
    )
    .await;
    assert_all(
        &GroupSupportingEffectController::default(),
        false,
        "a controller that implements all three",
    )
    .await;
}
