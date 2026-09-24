//! The group-head refusal a content-checked reopen gives (FIG-3586).

/// A content-checked reopen (FIG-3586) replays a recorded command: a shape it
/// no longer matches is that command's divergence, under the tier's
/// replay-mismatch code, so the run parks instead of failing.
pub(crate) fn content_checked_shape_mismatch(
    group_key: &str,
) -> lash_core::RuntimeEffectControllerError {
    lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::WorkerReplacementAbort,
        format!(
            "effect group {group_key} was reopened with a different durable shape than the \
             recorded one; the group head refuses a redrive whose aggregate differs from the \
             recorded one"
        ),
    )
}
