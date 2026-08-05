pub(super) fn drop_queue_settlement_row(
    claims: &mut Vec<crate::QueuedWorkCompletion>,
    claim_id: &str,
    row_id: &str,
) -> bool {
    let mut removed = false;
    for claim in claims.iter_mut().filter(|claim| claim.claim_id == claim_id) {
        let prior = claim.batch_ids.len();
        claim.batch_ids.retain(|batch_id| batch_id != row_id);
        removed |= claim.batch_ids.len() != prior;
    }
    claims.retain(|claim| !claim.batch_ids.is_empty());
    removed
}

pub(super) fn drop_turn_input_settlement_row(
    claims: &mut Vec<crate::TurnInputCompletion>,
    claim_id: &str,
    row_id: &str,
) -> bool {
    let mut removed = false;
    for claim in claims.iter_mut().filter(|claim| claim.claim_id == claim_id) {
        let prior = claim.input_ids.len();
        claim.input_ids.retain(|input_id| input_id != row_id);
        claim
            .applications
            .retain(|application| application.input_id != row_id);
        removed |= claim.input_ids.len() != prior;
    }
    claims.retain(|claim| !claim.input_ids.is_empty());
    removed
}

pub(super) fn drop_superseded_recovered_queue_settlement(
    error: &crate::StoreError,
    claim_generations: &std::collections::HashMap<String, u64>,
    current_session_lease_generation: Option<u64>,
    completed_claims: &mut Vec<crate::QueuedWorkCompletion>,
    originating_claims: &mut Vec<crate::QueuedWorkCompletion>,
) -> bool {
    let crate::StoreError::QueuedWorkClaimSuperseded {
        session_id,
        claim_id,
        row_id: Some(row_id),
        superseding_claim_id,
        superseding_session_lease_generation,
        ..
    } = error
    else {
        return false;
    };
    claim_generations.get(claim_id).is_some_and(|stale_generation| {
        let recovered = current_session_lease_generation
            .is_some_and(|current| *stale_generation < current);
        let removed = recovered
            && drop_queue_settlement_row(completed_claims, claim_id, row_id)
            && drop_queue_settlement_row(originating_claims, claim_id, row_id);
        if removed {
            tracing::warn!(
                target: "lash_core::claim_settlement",
                event = "claim_settlement.recovered_row_dropped",
                decision_basis = "superseded_recovered_claim",
                session_id,
                row_kind = "queued_work",
                row_id,
                stale_claim_id = claim_id,
                stale_session_lease_generation = *stale_generation,
                current_session_lease_generation,
                superseding_claim_id,
                superseding_session_lease_generation,
                outcome = "drop_stale_settlement",
                "recovered final commit dropped a queued-work row no longer owned by its restored claim"
            );
        }
        removed
    })
}

pub(super) fn drop_superseded_recovered_turn_input_settlement(
    error: &crate::StoreError,
    claim_generations: &std::collections::HashMap<String, u64>,
    current_session_lease_generation: Option<u64>,
    completed_claims: &mut Vec<crate::TurnInputCompletion>,
    originating_claims: &mut Vec<crate::TurnInputCompletion>,
) -> bool {
    let crate::StoreError::TurnInputClaimSuperseded {
        session_id,
        claim_id,
        row_id: Some(row_id),
        superseding_claim_id,
        superseding_session_lease_generation,
        ..
    } = error
    else {
        return false;
    };
    claim_generations.get(claim_id).is_some_and(|stale_generation| {
        let recovered = current_session_lease_generation
            .is_some_and(|current| *stale_generation < current);
        let removed = recovered
            && drop_turn_input_settlement_row(completed_claims, claim_id, row_id)
            && drop_turn_input_settlement_row(originating_claims, claim_id, row_id);
        if removed {
            tracing::warn!(
                target: "lash_core::claim_settlement",
                event = "claim_settlement.recovered_row_dropped",
                decision_basis = "superseded_recovered_claim",
                session_id,
                row_kind = "turn_input",
                row_id,
                stale_claim_id = claim_id,
                stale_session_lease_generation = *stale_generation,
                current_session_lease_generation,
                superseding_claim_id,
                superseding_session_lease_generation,
                outcome = "drop_stale_settlement",
                "recovered final commit dropped a turn-input row no longer owned by its restored claim"
            );
        }
        removed
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::tests::trace_capture::{CapturedFieldKind, EventCapture, capturing_sync};

    fn completion() -> crate::QueuedWorkCompletion {
        crate::QueuedWorkCompletion {
            session_id: "fig905".to_string(),
            claim_id: "stale-claim".to_string(),
            lease_token: "stale-token".to_string(),
            data: crate::QueuedWorkCompletionData {
                batch_ids: vec!["fig905-row".to_string()],
            },
        }
    }

    fn superseded_error(row_id: Option<Box<str>>) -> crate::StoreError {
        crate::StoreError::QueuedWorkClaimSuperseded {
            session_id: "fig905".to_string(),
            claim_id: "stale-claim".to_string(),
            row_id,
            superseding_claim_id: Some("live-claim".into()),
            superseding_session_lease_generation: Some(Box::new(2)),
        }
    }

    fn turn_input_completion() -> crate::TurnInputCompletion {
        crate::TurnInputCompletion {
            session_id: "fig905".to_string(),
            claim_id: "stale-claim".to_string(),
            lease_token: "stale-token".to_string(),
            data: crate::TurnInputCompletionData {
                input_ids: vec!["fig905-row".to_string()],
                applications: Vec::new(),
            },
        }
    }

    fn turn_input_superseded_error() -> crate::StoreError {
        crate::StoreError::TurnInputClaimSuperseded {
            session_id: "fig905".to_string(),
            claim_id: "stale-claim".to_string(),
            row_id: Some("fig905-row".into()),
            superseding_claim_id: Some("live-claim".into()),
            superseding_session_lease_generation: Some(Box::new(2)),
        }
    }

    #[test]
    fn recovered_settlement_escape_rejects_a_current_generation_conflict() {
        let mut completed = vec![completion()];
        let mut originating = vec![completion()];
        let generations = std::iter::once(("stale-claim".to_string(), 2)).collect();

        assert!(!drop_superseded_recovered_queue_settlement(
            &superseded_error(Some("fig905-row".into())),
            &generations,
            Some(2),
            &mut completed,
            &mut originating,
        ));
        assert_eq!(completed, vec![completion()]);
        assert_eq!(originating, vec![completion()]);
    }

    #[test]
    fn recovered_settlement_escape_rejects_an_error_without_a_row_id() {
        let mut completed = vec![completion()];
        let mut originating = vec![completion()];
        let generations = std::iter::once(("stale-claim".to_string(), 1)).collect();

        assert!(!drop_superseded_recovered_queue_settlement(
            &superseded_error(None),
            &generations,
            Some(2),
            &mut completed,
            &mut originating,
        ));
        assert_eq!(completed, vec![completion()]);
        assert_eq!(originating, vec![completion()]);
    }

    fn assert_recovered_drop_event(capture: &EventCapture, row_kind: &str, message: &str) {
        let event = capture.exactly_one("claim_settlement.recovered_row_dropped");
        assert_eq!(event.level, "WARN");
        assert_eq!(event.target, "lash_core::claim_settlement");
        let expected = [
            (
                "event",
                "claim_settlement.recovered_row_dropped",
                CapturedFieldKind::Str,
            ),
            (
                "decision_basis",
                "superseded_recovered_claim",
                CapturedFieldKind::Str,
            ),
            ("session_id", "fig905", CapturedFieldKind::Str),
            ("row_kind", row_kind, CapturedFieldKind::Str),
            ("row_id", "fig905-row", CapturedFieldKind::Str),
            ("stale_claim_id", "stale-claim", CapturedFieldKind::Str),
            (
                "stale_session_lease_generation",
                "1",
                CapturedFieldKind::U64,
            ),
            (
                "current_session_lease_generation",
                "2",
                CapturedFieldKind::U64,
            ),
            ("superseding_claim_id", "live-claim", CapturedFieldKind::Str),
            (
                "superseding_session_lease_generation",
                "2",
                CapturedFieldKind::U64,
            ),
            ("outcome", "drop_stale_settlement", CapturedFieldKind::Str),
            ("message", message, CapturedFieldKind::Debug),
        ];
        assert_eq!(
            event.field_count(),
            expected.len(),
            "event field set changed: {event:?}"
        );
        for (field, value, kind) in expected {
            assert_eq!(
                event.field_kind(field),
                kind,
                "settlement event field `{field}` encoding changed: {event:?}"
            );
            assert_eq!(
                event.field(field),
                value,
                "settlement event field `{field}` changed: {event:?}"
            );
        }
    }

    #[test]
    fn recovered_queue_settlement_drop_warns_with_typed_decision_basis() {
        let mut completed = vec![completion()];
        let mut originating = vec![completion()];
        let generations = std::iter::once(("stale-claim".to_string(), 1)).collect();

        let (removed, capture) = capturing_sync(|| {
            drop_superseded_recovered_queue_settlement(
                &superseded_error(Some("fig905-row".into())),
                &generations,
                Some(2),
                &mut completed,
                &mut originating,
            )
        });
        assert!(removed);
        assert_recovered_drop_event(
            &capture,
            "queued_work",
            "recovered final commit dropped a queued-work row no longer owned by its restored claim",
        );
    }

    #[test]
    fn recovered_turn_input_settlement_drop_warns_with_typed_decision_basis() {
        let mut completed = vec![turn_input_completion()];
        let mut originating = vec![turn_input_completion()];
        let generations = std::iter::once(("stale-claim".to_string(), 1)).collect();

        let (removed, capture) = capturing_sync(|| {
            drop_superseded_recovered_turn_input_settlement(
                &turn_input_superseded_error(),
                &generations,
                Some(2),
                &mut completed,
                &mut originating,
            )
        });
        assert!(removed);
        assert_recovered_drop_event(
            &capture,
            "turn_input",
            "recovered final commit dropped a turn-input row no longer owned by its restored claim",
        );
    }
}
