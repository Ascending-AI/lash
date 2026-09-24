use super::*;

/// A content-checked reopen whose shape drifted parks under its own effect
/// kind, not `unknown` (FIG-3587): the re-coded refusal names the group head.
#[test]
fn a_group_shape_mismatch_names_the_group_head_as_its_effect_kind() {
    let refused = as_replay_mismatch(
        group_shape_error("reopened with another shape"),
        crate::RuntimeErrorCode::SqliteEffectReplayHashConflict,
    );
    assert_eq!(
        refused.code,
        crate::RuntimeErrorCode::SqliteEffectReplayHashConflict
    );
    assert_eq!(
        refused
            .summary
            .as_deref()
            .and_then(|summary| summary.effect_kind.as_deref()),
        Some("effect_group")
    );
    let park = crate::store::TurnParkReason::of_error(&refused.into_runtime_error())
        .expect("a replay hash conflict parks");
    assert!(
        matches!(
            park,
            crate::store::TurnParkReason::EffectReplayDivergence { ref effect_kind, .. }
                if effect_kind == "effect_group"
        ),
        "{park:?}"
    );
}
