use crate::SessionId;
use std::collections::HashMap;

pub(super) struct ClaimSettlement<C> {
    pub(super) completions: Vec<C>,
    pub(super) generations: HashMap<String, u64>,
    #[cfg(test)]
    originating_override: Option<Vec<C>>,
}

impl<C> ClaimSettlement<C> {
    pub(super) fn new(completions: Vec<C>, generations: HashMap<String, u64>) -> Self {
        Self {
            completions,
            generations,
            #[cfg(test)]
            originating_override: None,
        }
    }

    pub(super) fn originating(&self) -> &[C] {
        #[cfg(test)]
        if let Some(originating) = &self.originating_override {
            return originating;
        }
        &self.completions
    }

    #[cfg(test)]
    pub(super) fn divergent_for_test(
        originating: Vec<C>,
        completions: Vec<C>,
        generations: HashMap<String, u64>,
    ) -> Self {
        Self {
            completions,
            generations,
            originating_override: Some(originating),
        }
    }
}

pub(super) struct TurnClaimSettlement {
    pub(super) queued: ClaimSettlement<crate::QueuedWorkCompletion>,
    pub(super) turn_inputs: ClaimSettlement<crate::TurnInputCompletion>,
}

impl TurnClaimSettlement {
    pub(super) fn new(
        queued: Vec<crate::QueuedWorkCompletion>,
        turn_inputs: Vec<crate::TurnInputCompletion>,
        queue_generations: HashMap<String, u64>,
        input_generations: HashMap<String, u64>,
    ) -> Self {
        Self {
            queued: ClaimSettlement::new(queued, queue_generations),
            turn_inputs: ClaimSettlement::new(turn_inputs, input_generations),
        }
    }

    pub(super) fn has_recovered(&self, current: Option<u64>) -> bool {
        current.is_some_and(|current| {
            self.queued
                .generations
                .values()
                .chain(self.turn_inputs.generations.values())
                .any(|generation| *generation < current)
        })
    }

    pub(super) fn drop_superseded(
        &mut self,
        error: &crate::StoreError,
        current: Option<u64>,
    ) -> bool {
        self.queued.drop_superseded(error, current)
            || self.turn_inputs.drop_superseded(error, current)
    }

    #[cfg(test)]
    pub(super) fn for_test(
        originating_queue: Vec<crate::QueuedWorkCompletion>,
        originating_inputs: Vec<crate::TurnInputCompletion>,
        completed_queue: Vec<crate::QueuedWorkCompletion>,
        completed_inputs: Vec<crate::TurnInputCompletion>,
        queue_generations: HashMap<String, u64>,
        input_generations: HashMap<String, u64>,
    ) -> Self {
        Self {
            queued: ClaimSettlement::divergent_for_test(
                originating_queue,
                completed_queue,
                queue_generations,
            ),
            turn_inputs: ClaimSettlement::divergent_for_test(
                originating_inputs,
                completed_inputs,
                input_generations,
            ),
        }
    }
}

pub(super) struct Supersession<'a> {
    session_id: &'a SessionId,
    claim_id: &'a str,
    row_id: &'a str,
    superseding_claim_id: &'a Option<Box<str>>,
    superseding_generation: &'a Option<Box<u64>>,
}

pub(super) trait SettlementRows {
    const ROW_KIND: &'static str;
    const MESSAGE: &'static str;
    fn claim_id(&self) -> Option<&str>;
    fn rows(&self) -> &[String];
    fn remove_row(&mut self, row_id: &str);
    fn supersession(error: &crate::StoreError) -> Option<Supersession<'_>>;
}

fn drop_row<C: SettlementRows>(claims: &mut Vec<C>, claim_id: &str, row_id: &str) {
    for claim in claims
        .iter_mut()
        .filter(|claim| claim.claim_id() == Some(claim_id))
    {
        claim.remove_row(row_id);
    }
    claims.retain(|claim| !claim.rows().is_empty());
}

impl<C: SettlementRows> ClaimSettlement<C> {
    fn drop_superseded(
        &mut self,
        error: &crate::StoreError,
        current_session_lease_generation: Option<u64>,
    ) -> bool {
        let Some(Supersession {
            session_id,
            claim_id,
            row_id,
            superseding_claim_id,
            superseding_generation: superseding_session_lease_generation,
        }) = C::supersession(error)
        else {
            return false;
        };
        let Some(&stale_generation) = self.generations.get(claim_id) else {
            return false;
        };
        let holds_row = |claim: &C| {
            claim.claim_id() == Some(claim_id) && claim.rows().iter().any(|id| id == row_id)
        };
        if !current_session_lease_generation.is_some_and(|current| stale_generation < current)
            || !self.completions.iter().any(holds_row)
            || !self.originating().iter().any(holds_row)
        {
            return false;
        }
        drop_row(&mut self.completions, claim_id, row_id);
        #[cfg(test)]
        if let Some(originating) = &mut self.originating_override {
            drop_row(originating, claim_id, row_id);
        }
        tracing::warn!(
            target: "lash_core::claim_settlement",
            event = "claim_settlement.recovered_row_dropped",
            decision_basis = "superseded_recovered_claim",
            session_id = %session_id,
            row_kind = C::ROW_KIND,
            row_id,
            stale_claim_id = claim_id,
            stale_session_lease_generation = stale_generation,
            current_session_lease_generation,
            superseding_claim_id,
            superseding_session_lease_generation,
            outcome = "drop_stale_settlement",
            "{}", C::MESSAGE
        );
        true
    }
}

impl SettlementRows for crate::QueuedWorkCompletion {
    const ROW_KIND: &'static str = "queued_work";
    const MESSAGE: &'static str =
        "recovered final commit dropped a queued-work row no longer owned by its restored claim";
    fn claim_id(&self) -> Option<&str> {
        Some(&self.claim_id)
    }
    fn rows(&self) -> &[String] {
        &self.batch_ids
    }
    fn remove_row(&mut self, row_id: &str) {
        self.batch_ids.retain(|id| id != row_id);
    }
    fn supersession(error: &crate::StoreError) -> Option<Supersession<'_>> {
        match error {
            crate::StoreError::QueuedWorkClaimSuperseded {
                session_id,
                claim_id,
                row_id: Some(row_id),
                superseding_claim_id,
                superseding_session_lease_generation,
            } => Some(Supersession {
                session_id,
                claim_id,
                row_id,
                superseding_claim_id,
                superseding_generation: superseding_session_lease_generation,
            }),
            _ => None,
        }
    }
}

impl SettlementRows for crate::TurnInputCompletion {
    const ROW_KIND: &'static str = "turn_input";
    const MESSAGE: &'static str =
        "recovered final commit dropped a turn-input row no longer owned by its restored claim";
    fn claim_id(&self) -> Option<&str> {
        self.claim_id()
    }
    fn rows(&self) -> &[String] {
        &self.input_ids
    }
    fn remove_row(&mut self, row_id: &str) {
        self.input_ids.retain(|id| id != row_id);
        self.applications
            .retain(|application| application.input_id != row_id);
    }
    fn supersession(error: &crate::StoreError) -> Option<Supersession<'_>> {
        match error {
            crate::StoreError::TurnInputClaimSuperseded {
                session_id,
                claim_id,
                row_id: Some(row_id),
                superseding_claim_id,
                superseding_session_lease_generation,
            } => Some(Supersession {
                session_id,
                claim_id,
                row_id,
                superseding_claim_id,
                superseding_generation: superseding_session_lease_generation,
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn drop_for_test<C: SettlementRows + Clone>(
        error: &crate::StoreError,
        generations: &HashMap<String, u64>,
        current: Option<u64>,
        completed: &mut Vec<C>,
        originating: &mut Vec<C>,
    ) -> bool {
        let mut settlement = ClaimSettlement::divergent_for_test(
            originating.clone(),
            completed.clone(),
            generations.clone(),
        );
        let removed = settlement.drop_superseded(error, current);
        *originating = settlement.originating().to_vec();
        *completed = settlement.completions;
        removed
    }
    use crate::runtime::tests::trace_capture::{CapturedFieldKind, EventCapture, capturing_sync};

    fn completion() -> crate::QueuedWorkCompletion {
        crate::QueuedWorkCompletion {
            session_id: SessionId::from("fig905"),
            claim_id: "stale-claim".to_string(),
            lease_token: "stale-token".to_string(),
            data: crate::QueuedWorkCompletionData {
                batch_ids: vec!["fig905-row".to_string()],
            },
        }
    }

    fn superseded_error(row_id: Option<Box<str>>) -> crate::StoreError {
        crate::StoreError::QueuedWorkClaimSuperseded {
            session_id: SessionId::from("fig905"),
            claim_id: "stale-claim".to_string(),
            row_id,
            superseding_claim_id: Some("live-claim".into()),
            superseding_session_lease_generation: Some(Box::new(2)),
        }
    }

    fn turn_input_completion() -> crate::TurnInputCompletion {
        crate::TurnInputCompletion {
            session_id: SessionId::from("fig905"),
            claim: Some(crate::TurnInputSettlementClaim {
                claim_id: "stale-claim".to_string(),
                lease_token: "stale-token".to_string(),
            }),
            data: crate::TurnInputCompletionData {
                input_ids: vec!["fig905-row".to_string()],
                applications: Vec::new(),
            },
        }
    }

    fn turn_input_superseded_error() -> crate::StoreError {
        crate::StoreError::TurnInputClaimSuperseded {
            session_id: SessionId::from("fig905"),
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

        assert!(!drop_for_test(
            &superseded_error(Some("fig905-row".into())),
            &generations,
            Some(2),
            &mut completed,
            &mut originating,
        ));
        assert_eq!(completed, vec![completion()]);
        assert_eq!(originating, vec![completion()]);
    }

    fn foreign_row_completion() -> crate::QueuedWorkCompletion {
        crate::QueuedWorkCompletion {
            session_id: SessionId::from("fig905"),
            claim_id: "stale-claim".to_string(),
            lease_token: "stale-token".to_string(),
            data: crate::QueuedWorkCompletionData {
                batch_ids: vec!["fig905-other-row".to_string()],
            },
        }
    }

    fn foreign_row_turn_input_completion() -> crate::TurnInputCompletion {
        crate::TurnInputCompletion {
            session_id: SessionId::from("fig905"),
            claim: Some(crate::TurnInputSettlementClaim {
                claim_id: "stale-claim".to_string(),
                lease_token: "stale-token".to_string(),
            }),
            data: crate::TurnInputCompletionData {
                input_ids: vec!["fig905-other-row".to_string()],
                applications: Vec::new(),
            },
        }
    }

    #[test]
    fn recovered_queue_settlement_escape_mutates_nothing_when_only_one_side_holds_the_row() {
        let mut completed = vec![completion()];
        let mut originating = vec![foreign_row_completion()];
        let generations = std::iter::once(("stale-claim".to_string(), 1)).collect();

        assert!(!drop_for_test(
            &superseded_error(Some("fig905-row".into())),
            &generations,
            Some(2),
            &mut completed,
            &mut originating,
        ));
        assert_eq!(completed, vec![completion()]);
        assert_eq!(originating, vec![foreign_row_completion()]);
    }

    #[test]
    fn recovered_turn_input_settlement_escape_mutates_nothing_when_only_one_side_holds_the_row() {
        let mut completed = vec![turn_input_completion()];
        let mut originating = vec![foreign_row_turn_input_completion()];
        let generations = std::iter::once(("stale-claim".to_string(), 1)).collect();

        assert!(!drop_for_test(
            &turn_input_superseded_error(),
            &generations,
            Some(2),
            &mut completed,
            &mut originating,
        ));
        assert_eq!(completed, vec![turn_input_completion()]);
        assert_eq!(originating, vec![foreign_row_turn_input_completion()]);
    }

    #[test]
    fn recovered_settlement_escape_rejects_an_error_without_a_row_id() {
        let mut completed = vec![completion()];
        let mut originating = vec![completion()];
        let generations = std::iter::once(("stale-claim".to_string(), 1)).collect();

        assert!(!drop_for_test(
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
            drop_for_test(
                &superseded_error(Some("fig905-row".into())),
                &generations,
                Some(2),
                &mut completed,
                &mut originating,
            )
        });
        assert!(removed);
        assert!(completed.is_empty());
        assert_eq!(completed, originating);
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
            drop_for_test(
                &turn_input_superseded_error(),
                &generations,
                Some(2),
                &mut completed,
                &mut originating,
            )
        });
        assert!(removed);
        assert!(completed.is_empty());
        assert_eq!(completed, originating);
        assert_recovered_drop_event(
            &capture,
            "turn_input",
            "recovered final commit dropped a turn-input row no longer owned by its restored claim",
        );
    }
    #[test]
    fn claim_settlement_derived_views_agree_after_each_kind_drops() {
        let generations: HashMap<String, u64> = [("stale-claim".into(), 1)].into_iter().collect();
        let mut queued = ClaimSettlement::new(vec![completion()], generations.clone());
        assert!(queued.drop_superseded(&superseded_error(Some("fig905-row".into())), Some(2)));
        assert!(queued.completions.is_empty());
        assert_eq!(queued.originating(), queued.completions.as_slice());
        let mut inputs = ClaimSettlement::new(vec![turn_input_completion()], generations);
        assert!(inputs.drop_superseded(&turn_input_superseded_error(), Some(2)));
        assert!(inputs.completions.is_empty());
        assert_eq!(inputs.originating(), inputs.completions.as_slice());
    }
}
