//! The group-head refusals a reopen that diverged from its recorded run gives
//! (FIG-3586).
//!
//! Both carry the engine-neutral divergence code and name the diverged effect
//! as the group head (`effect_group`), so the turn parks under that kind: the
//! engine keeps the invocation's journal, and a redeploy of the build that
//! wrote it serves the redrive.

/// A content-checked reopen (FIG-3586) replays a recorded command: a shape it
/// no longer matches is that command's divergence.
pub(crate) fn content_checked_shape_mismatch(
    group_key: &str,
) -> lash_core::RuntimeEffectControllerError {
    group_divergence(format!(
        "effect group {group_key} was reopened with a different durable shape than the \
         recorded one; the group head refuses a redrive whose aggregate differs from the \
         recorded one"
    ))
}

/// A reopen whose child at `position` is not the retained one.
pub(crate) fn content_mismatch(
    group_key: &str,
    position: impl std::fmt::Display,
) -> lash_core::RuntimeEffectControllerError {
    group_divergence(format!(
        "effect group {group_key} was reopened with a child at position {position} that is \
         not the retained one; the group head refuses a redrive whose aggregate differs from \
         the recorded one"
    ))
}

fn group_divergence(message: String) -> lash_core::RuntimeEffectControllerError {
    lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::EffectReplayDivergence,
        message,
    )
    .with_summary(lash_core::RuntimeEffectReplayMismatchReport {
        divergent_path_count: 1,
        first_divergent_paths: vec!["group".to_string()],
        effect_kind: Some("effect_group".to_string()),
    })
}
