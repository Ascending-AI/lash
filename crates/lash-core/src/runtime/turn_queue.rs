pub use lash_core_store::queued_work_vocabulary::*;
use super::process::ProcessWakeDelivery;
use crate::ProcessId;
use crate::SessionId;
use crate::store::QueuedWorkClass;
use crate::{PluginMessage, TurnCause, TurnInput};









/// An accepted session command waiting for its queue completion to be
/// committed atomically with the new session head.
#[derive(Clone, Debug)]
pub(crate) struct SessionCommandSettlementHandle {
    pub(crate) receipt: SessionCommandReceipt,
}







/// Constant producer-selected merge key for process wakes.
///
/// The key says only that wake rows are eligible to share a turn. Work kind,
/// delivery boundary, authority, elevation, row count, age, and rendered size
/// remain independent claim gates.
pub const PROCESS_WAKE_MERGE_KEY: &str = "lash.process_wake";









/// Host policy bounding one automatically selected queued-work claim.
///
/// Row count and maximum pending age retain Lash defaults because a poor
/// choice affects batching efficiency rather than context-window correctness.
/// The action reserve is required and has no Lash default; since FIG-1313 it is
/// advisory evidence for a custom [`QueuedDrainPolicy`](crate::QueuedDrainPolicy)
/// plus a misconfiguration check, because no shipped drain mode does token
/// arithmetic (see [`action_token_reserve`](Self::action_token_reserve)).
///
/// How much of the legal, FIFO-ordered claimable prefix actually drains on one
/// wake is a separate, explicitly named host policy: the
/// [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) selected by
/// [`with_drain_mode`](Self::with_drain_mode) or
/// [`with_drain_policy`](Self::with_drain_policy), defaulting to
/// [`DrainMode::OneAtATime`](crate::DrainMode::OneAtATime).
#[derive(Clone, Debug)]
pub struct QueuedWorkBatchingConfig {
    action_token_reserve: std::num::NonZeroUsize,
    max_rows: std::num::NonZeroUsize,
    max_pending_age: std::time::Duration,
    /// `None` selects the documented Lash default,
    /// [`DrainMode::OneAtATime`](crate::DrainMode::OneAtATime), so the
    /// configuration stays `const`-constructible.
    drain_policy: Option<std::sync::Arc<dyn crate::QueuedDrainPolicy>>,
}

impl PartialEq for QueuedWorkBatchingConfig {
    /// Compares the scalar bounds and the *resolved* drain policy by identity.
    ///
    /// Resolving first keeps the shipped modes honest: an unset policy and an
    /// explicit [`with_drain_mode`](Self::with_drain_mode) naming the same mode
    /// share one instance and compare equal. Two separately constructed *custom*
    /// policies never do, even when they behave identically — Lash cannot prove
    /// that, so it does not claim it. Hosts that compare configurations holding
    /// custom policies should share one `Arc`.
    fn eq(&self, other: &Self) -> bool {
        self.action_token_reserve == other.action_token_reserve
            && self.max_rows == other.max_rows
            && self.max_pending_age == other.max_pending_age
            && std::sync::Arc::ptr_eq(&self.drain_policy(), &other.drain_policy())
    }
}

impl QueuedWorkBatchingConfig {
    /// Default upper bound on rows coalesced into one fresh turn claim.
    ///
    /// Hosts may replace this efficiency bound with [`Self::with_max_rows`].
    /// Interrupted-claim redrive preserves the predecessor composition even
    /// when it contains more rows than this fresh-claim default.
    pub const DEFAULT_MAX_ROWS: usize = 64;
    /// Default age at which the oldest compatible row is claimed alone instead
    /// of being coalesced with later ready rows.
    ///
    /// Hosts may replace this latency bound with
    /// [`Self::with_max_pending_age`].
    pub const DEFAULT_MAX_PENDING_AGE: std::time::Duration = std::time::Duration::from_secs(30);

    /// Construct batching policy with an explicit non-zero model-action
    /// reserve and defaulted row-count and pending-age bounds.
    ///
    /// These bounds apply to fresh claims. Redriving an interrupted claim keeps
    /// its already-journaled composition intact.
    ///
    /// # Panics
    ///
    /// Panics when `action_token_reserve` is zero.
    #[expect(
        clippy::expect_used,
        reason = "the default queued-work row bound is a non-zero literal"
    )]
    pub const fn new(action_token_reserve: usize) -> Self {
        let Some(action_token_reserve) = std::num::NonZeroUsize::new(action_token_reserve) else {
            panic!("queued-work action token reserve must be non-zero");
        };
        Self {
            action_token_reserve,
            max_rows: std::num::NonZeroUsize::new(Self::DEFAULT_MAX_ROWS)
                .expect("default queued-work row bound is non-zero"),
            max_pending_age: Self::DEFAULT_MAX_PENDING_AGE,
            drain_policy: None,
        }
    }

    /// Selects one of the two shipped drain shapes.
    ///
    /// Unset, Lash uses [`DrainMode::OneAtATime`](crate::DrainMode::OneAtATime):
    /// one queued row per drain, strict FIFO, no token arithmetic.
    pub fn with_drain_mode(mut self, mode: crate::DrainMode) -> Self {
        self.drain_policy = Some(crate::runtime::shared_drain_mode_policy(mode));
        self
    }

    /// Installs a fully custom [`QueuedDrainPolicy`](crate::QueuedDrainPolicy).
    ///
    /// This is the escape hatch for hosts wanting selection Lash deliberately
    /// does not ship, such as window-fitted prefix selection.
    pub fn with_drain_policy(
        mut self,
        drain_policy: std::sync::Arc<dyn crate::QueuedDrainPolicy>,
    ) -> Self {
        self.drain_policy = Some(drain_policy);
        self
    }

    /// Returns the configured drain policy, or the Lash default when the host
    /// selected none.
    pub fn drain_policy(&self) -> std::sync::Arc<dyn crate::QueuedDrainPolicy> {
        self.drain_policy
            .clone()
            .unwrap_or_else(crate::default_queued_drain_policy)
    }

    /// Sets the maximum number of compatible rows coalesced into one fresh
    /// claim. Redriving an interrupted claim may exceed this bound to preserve
    /// its already-journaled composition.
    ///
    /// # Panics
    ///
    /// Panics when `max_rows` is zero.
    pub const fn with_max_rows(mut self, max_rows: usize) -> Self {
        let Some(max_rows) = std::num::NonZeroUsize::new(max_rows) else {
            panic!("queued-work max rows must be non-zero");
        };
        self.max_rows = max_rows;
        self
    }

    /// Sets the age at which Lash claims the oldest compatible row alone
    /// instead of coalescing it with later ready rows.
    ///
    /// # Panics
    ///
    /// Panics when `max_pending_age` is zero.
    pub const fn with_max_pending_age(mut self, max_pending_age: std::time::Duration) -> Self {
        assert!(
            !max_pending_age.is_zero(),
            "queued-work max pending age must be non-zero"
        );
        self.max_pending_age = max_pending_age;
        self
    }

    /// Returns the context capacity reserved for the model action after Lash
    /// renders the queued-work prefix.
    ///
    /// Since FIG-1313 no shipped drain policy spends this budget: the two
    /// default modes do no token arithmetic. It is handed to the configured
    /// [`QueuedDrainPolicy`](crate::QueuedDrainPolicy) as
    /// [`QueuedDrainRequest::available_tokens`](crate::QueuedDrainRequest::available_tokens),
    /// where a custom policy may weigh it, and a reserve that consumes the
    /// whole model context is still rejected as a misconfiguration. A row
    /// larger than the entire context is refused and left pending.
    pub const fn action_token_reserve(&self) -> usize {
        self.action_token_reserve.get()
    }

    /// Returns the maximum number of compatible rows in one fresh claim.
    ///
    /// This does not split an interrupted claim whose complete composition
    /// must be redriven atomically.
    pub const fn max_rows(&self) -> usize {
        self.max_rows.get()
    }

    /// Returns the age at which the oldest compatible row is claimed alone.
    ///
    /// This is a batching-latency bound, not a queue expiry: reaching it does
    /// not discard the row.
    pub const fn max_pending_age(&self) -> std::time::Duration {
        self.max_pending_age
    }

    pub(crate) fn claim_policy(&self, max_context_tokens: usize) -> QueuedWorkClaimPolicy {
        QueuedWorkClaimPolicy {
            max_context_tokens,
            action_token_reserve: self.action_token_reserve(),
            max_rows: self.max_rows(),
            max_pending_age_ms: u64::try_from(self.max_pending_age.as_millis()).unwrap_or(u64::MAX),
            drain_policy: self.drain_policy(),
        }
    }
}







































pub fn process_wake_batch_draft(wake: ProcessWakeDelivery) -> QueuedWorkBatchDraft {
    process_wake_batch_draft_with_delivery_policy(wake, DeliveryPolicy::EarliestSafeBoundary)
}

/// Draft a process wake using the host-selected delivery boundary.
///
/// Delivery timing is independent of merge eligibility: it remains a selector
/// compatibility gate and is never encoded into the merge key.
pub fn process_wake_batch_draft_with_delivery_policy(
    wake: ProcessWakeDelivery,
    delivery_policy: DeliveryPolicy,
) -> QueuedWorkBatchDraft {
    let source_key = process_wake_source_key(&wake.process_id, wake.sequence);
    let process_id = wake.process_id.clone();
    let sequence = wake.sequence;
    let authority = wake.authority.clone();
    QueuedWorkBatchDraft::new(
        wake.target_session_id.clone(),
        delivery_policy,
        crate::TurnWorkPayload::process_wake(wake),
    )
    .with_source_key(source_key)
    .with_process_wake_source(process_id, sequence)
    .with_authority(authority)
    .with_merge_key(PROCESS_WAKE_MERGE_KEY)
}



#[cfg(test)]
mod wire_tests {
    use super::{DeliveryPolicy, QueuedWorkKind};

    #[test]
    fn queued_work_wire_values_match_the_persisted_ingress_encoding() {
        assert_eq!(QueuedWorkKind::Turn.as_str(), "turn");
        assert_eq!(QueuedWorkKind::Control.as_str(), "control");
        assert_eq!(
            QueuedWorkKind::from_wire_str("turn"),
            Some(QueuedWorkKind::Turn)
        );
        assert_eq!(
            QueuedWorkKind::from_wire_str("control"),
            Some(QueuedWorkKind::Control)
        );
        assert_eq!(QueuedWorkKind::from_wire_str("cancel"), None);
    }

    #[test]
    fn queued_work_delivery_policy_wire_values_match_the_persisted_ingress_encoding() {
        assert_eq!(
            DeliveryPolicy::EarliestSafeBoundary.as_str(),
            "earliest_safe_boundary"
        );
        assert_eq!(
            DeliveryPolicy::AfterCurrentTurnCommit.as_str(),
            "after_current_turn_commit"
        );
    }
}































#[cfg(test)]
mod typed_payload_tests {
    use super::*;

    #[test]
    fn queued_work_typed_payloads_preserve_command_wire_shape() {
        let draft = QueuedWorkBatchDraft::new(
            "s",
            DeliveryPolicy::EarliestSafeBoundary,
            SessionCommand::RefreshToolCatalog {
                reason: "refresh".into(),
            },
        );
        let expected = serde_json::json!({
            "session_id": "s", "delivery_policy": "earliest_safe_boundary", "kind": "control",
            "authority": {}, "available_at_ms": 0,
            "payloads": [{"type": "session_command", "command": {"kind": "refresh_tool_catalog", "reason": "refresh"}}]
        });
        assert_eq!(serde_json::to_value(&draft).unwrap(), expected);
        let restored: QueuedWorkBatchDraft = serde_json::from_value(expected).unwrap();
        assert_eq!(restored.kind(), QueuedWorkKind::Control);
    }

    #[test]
    fn queued_work_typed_payloads_reject_empty_mixed_and_multiple_commands() {
        let command = serde_json::json!({"type": "session_command", "command": {"kind": "refresh_tool_catalog", "reason": "refresh"}});
        let turn = serde_json::to_value(QueuedWorkPayload::agent_frame_task(
            crate::facade_support::frame_node_id(&SessionId::from("s"), "f"),
            "task",
            None,
        ))
        .unwrap();
        for payloads in [
            serde_json::json!([]),
            serde_json::json!([command.clone(), turn.clone()]),
            serde_json::json!([turn, command.clone()]),
            serde_json::json!([command.clone(), command]),
        ] {
            assert!(serde_json::from_value::<QueuedWorkBatchPayloads>(payloads).is_err());
        }
    }
    #[test]
    fn queued_work_typed_draft_preserves_json_and_messagepack_bytes() {
        // Independent pin of the pre-cutover draft envelope and raw item array.
        #[derive(serde::Serialize)]
        struct WireDraft<'a> {
            session_id: &'a SessionId,
            #[serde(skip_serializing_if = "Option::is_none")]
            source_key: Option<&'a str>,
            #[serde(skip_serializing_if = "Option::is_none")]
            process_wake_source: Option<&'a ProcessWakeSource>,
            delivery_policy: DeliveryPolicy,
            kind: QueuedWorkKind,
            authority: &'a QueuedWorkAuthority,
            #[serde(skip_serializing_if = "Option::is_none")]
            merge_key: Option<&'a str>,
            available_at_ms: u64,
            payloads: Vec<QueuedWorkPayload>,
        }
        let command = SessionCommand::RefreshToolCatalog {
            reason: "wire-pin".into(),
        };
        let turn = QueuedWorkPayload::agent_frame_task(
            crate::facade_support::frame_node_id(&SessionId::from("s"), "f"),
            "task",
            None,
        );
        let mut command_draft =
            QueuedWorkBatchDraft::new("s", DeliveryPolicy::EarliestSafeBoundary, command.clone());
        command_draft.source_key = Some("source".into());
        command_draft.merge_key = Some("merge".into());
        let turn_draft = QueuedWorkBatchDraft::new(
            "s",
            DeliveryPolicy::EarliestSafeBoundary,
            QueuedWorkBatchPayloads::TurnWork {
                first: TurnWorkPayload(turn.clone()),
                rest: vec![TurnWorkPayload(turn.clone())],
            },
        );
        for (draft, kind, payloads) in [
            (
                command_draft,
                QueuedWorkKind::Control,
                vec![QueuedWorkPayload::session_command(command)],
            ),
            (turn_draft, QueuedWorkKind::Turn, vec![turn.clone(), turn]),
        ] {
            let wire = WireDraft {
                session_id: &draft.session_id,
                source_key: draft.source_key.as_deref(),
                process_wake_source: draft.process_wake_source.as_ref(),
                delivery_policy: draft.delivery_policy,
                kind,
                authority: &draft.authority,
                merge_key: draft.merge_key.as_deref(),
                available_at_ms: draft.available_at_ms,
                payloads,
            };
            assert_eq!(
                serde_json::to_vec(&draft).unwrap(),
                serde_json::to_vec(&wire).unwrap()
            );
            assert_eq!(
                rmp_serde::to_vec(&draft).unwrap(),
                rmp_serde::to_vec(&wire).unwrap()
            );
            let bytes = rmp_serde::to_vec_named(&wire).unwrap();
            assert_eq!(rmp_serde::to_vec_named(&draft).unwrap(), bytes);
            let restored: QueuedWorkBatchDraft = rmp_serde::from_slice(&bytes).unwrap();
            assert_eq!(restored.kind(), kind);
        }
    }
}
