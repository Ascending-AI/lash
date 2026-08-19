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
    inline: InlineRuntimeEffectController,
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
        self.inline.execute_effect(envelope, local_executor).await
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
    inline: InlineRuntimeEffectController,
    /// The disposition the open declared, so the close can resolve against it
    /// rather than accept whatever it is handed.
    declared: std::sync::Mutex<Option<crate::LoserDisposition>>,
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
        self.inline.execute_effect(envelope, local_executor).await
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
        disposition: crate::LoserDisposition,
    ) -> Result<(), RuntimeEffectControllerError> {
        let declared = self
            .declared
            .lock()
            .expect("declared disposition")
            .unwrap_or(disposition);
        crate::LoserDisposition::resolve_close(declared, disposition)?;
        Ok(())
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
        crate::LoserDisposition::Cancel,
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
        .close_effect_group(handle, crate::LoserDisposition::Cancel)
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
}
