mod tests {
    use lash_core_execution::ProcessStatus;
    use lash_core_execution::SessionId;
    use lash_core_execution::plugin::PluginError;
    use lash_core_execution::runtime::process::registry_transitions::*;
    use lash_core_execution::runtime::{
        PROCESS_WAKE_DELIVERY_FORMAT_VERSION, WakeDeliveryDisposition, WakeDeliveryState,
        WakeDiscardReason,
    };

    /// Every [`ProcessStatus`] variant, exhaustively, so the retention-label law can
    /// partition the enum instead of a hand-kept sample of it.
    ///
    /// `ProcessStatus::ALL` is generated from the same declaration as
    /// `ProcessStatus::label`, whose match has no wildcard arm: adding a variant
    /// stops the crate compiling until it is spelled there, which extends `ALL`,
    /// and the law test below then decides whether [`LIVE_PROCESS_STATUS_LABELS`]
    /// or [`RETIRED_PROCESS_STATUS_LABELS`] must grow with it.
    fn all_process_statuses() -> Vec<ProcessStatus> {
        // `ProcessStatus::ALL` is generated alongside `ProcessStatus::label` from one
        // declaration (FIG-2844), so it cannot fall behind the enum the way this
        // hand-kept list could.
        ProcessStatus::ALL.to_vec()
    }

    #[test]
    fn a_retained_tombstone_reports_the_pruned_refusal() {
        let error = absent_process_error(
            &lash_core_execution::ProcessId::fixture("process"),
            Some(ProcessTombstoneStamp {
                terminal_label: "completed".to_string(),
                pruned_at_ms: 4_242,
            }),
        );
        match error {
            PluginError::ProcessNoLongerRetained {
                terminal_label,
                pruned_at_ms,
            } => {
                assert_eq!(terminal_label, "completed");
                assert_eq!(pruned_at_ms, 4_242);
            }
            other => panic!("unexpected refusal: {other}"),
        }
    }

    #[test]
    fn an_unknown_process_id_reports_the_unknown_refusal() {
        let error = absent_process_error(&lash_core_execution::ProcessId::fixture("process"), None);
        assert!(
            matches!(
                &error,
                PluginError::ProcessUnknown { process_id } if process_id == lash_core_execution::ProcessId::fixture("process")
            ),
            "unexpected refusal: {error}"
        );
        assert!(
            matches!(
                unknown_process(&lash_core_execution::ProcessId::fixture("other")),
                PluginError::ProcessUnknown { process_id } if process_id == lash_core_execution::ProcessId::fixture("other")
            ),
            "unknown_process must preserve the refused id"
        );
    }

    #[test]
    fn process_status_labels_partition_live_from_retired() {
        let statuses = all_process_statuses();
        assert_eq!(
            statuses.len(),
            7,
            "every ProcessStatus variant must be listed for the partition to be a law"
        );
        let live: Vec<&'static str> = statuses
            .iter()
            .filter(|status| !status.is_retired())
            .map(ProcessStatus::label)
            .collect();
        assert_eq!(
            live,
            LIVE_PROCESS_STATUS_LABELS.to_vec(),
            "the registries' live SQL is written against exactly these labels"
        );
        let retired: Vec<&'static str> = statuses
            .iter()
            .filter(|status| status.is_retired())
            .map(ProcessStatus::label)
            .collect();
        assert_eq!(
            retired,
            RETIRED_PROCESS_STATUS_LABELS.to_vec(),
            "the registries' retention SQL reclaims exactly these labels"
        );
        for label in &retired {
            assert!(
                !LIVE_PROCESS_STATUS_LABELS.contains(label),
                "retired label `{label}` must not be treated as live"
            );
        }
        assert_eq!(
            live.len() + retired.len(),
            statuses.len(),
            "the partition must be total"
        );
    }

    #[test]
    fn caller_departed_is_retired_without_being_terminal() {
        // The refusal this ticket ratifies, as a law: a caller-departed row is
        // reclaimable by retention and yet never carries a terminal outcome
        // claim, because lash cannot observe one.
        assert!(!ProcessStatus::CallerDeparted.is_terminal());
        assert!(ProcessStatus::CallerDeparted.is_retired());
        assert!(
            RETIRED_PROCESS_STATUS_LABELS.contains(&ProcessStatus::CallerDeparted.label()),
            "retention must be able to reclaim a row nothing may terminalize"
        );
        assert!(!LIVE_PROCESS_STATUS_LABELS.contains(&ProcessStatus::CallerDeparted.label()));
    }

    // --- C1 / C2 / C3: wake reconciliation vocabulary ---------------------

    #[test]
    fn every_wake_delivery_state_round_trips_its_label() {
        for state in [
            WakeDeliveryState::Pending,
            WakeDeliveryState::Enqueuing,
            WakeDeliveryState::Enqueued,
            WakeDeliveryState::Discarded,
        ] {
            assert_eq!(
                wake_delivery_state_from_label("delivery", state.as_str())
                    .expect("a label this crate wrote must parse"),
                state
            );
        }
    }

    #[test]
    fn an_unknown_state_label_is_refused() {
        let error = wake_delivery_state_from_label("delivery", "settled")
            .expect_err("an unrecognised state is never defaulted");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message == "wake delivery `delivery` has unknown state `settled`"),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn every_discard_reason_round_trips_its_label() {
        for reason in [
            WakeDiscardReason::Expired,
            WakeDiscardReason::TargetGone,
            WakeDiscardReason::Retargeted,
            WakeDiscardReason::SequenceRewound,
        ] {
            assert_eq!(
                wake_discard_reason_from_label("delivery", Some(reason.as_str()))
                    .expect("a label this crate wrote must parse"),
                Some(reason)
            );
        }
        assert_eq!(
            wake_discard_reason_from_label("delivery", None).expect("no reason is not an error"),
            None,
            "a delivery that was never discarded carries no reason"
        );
    }

    #[test]
    fn an_unknown_discard_reason_label_is_refused() {
        let error = wake_discard_reason_from_label("delivery", Some("bored"))
            .expect_err("an unrecognised reason is never defaulted");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message == "wake delivery `delivery` has unknown discard reason `bored`"),
            "unexpected refusal: {error}"
        );
    }

    fn wake_delivery_json() -> String {
        let wake = lash_core_execution::runtime::ProcessWakeDelivery {
            version: PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            wake_id: format!("wake:v1:sha256:{}", "a".repeat(64)),
            target_session_id: SessionId::from("session"),
            process_id: lash_core_execution::ProcessId::fixture("process"),
            sequence: 4,
            event_type: "process.wake".to_string(),
            event_invocation: lash_core_execution::RuntimeInvocation::effect(
                lash_core_execution::EffectAddress::new(
                    lash_core_execution::ExecutionScope::process(
                        lash_core_execution::ProcessId::fixture("process"),
                    ),
                    "replay",
                )
                .expect("valid wake address"),
                lash_core_execution::RuntimeAttribution::none(),
                "effect",
            ),
            process_caused_by: None,
            authority: lash_core_execution::QueuedWorkAuthority::default(),
            input: "wake".to_string(),
            created_at_ms: 10,
        };
        serde_json::to_string(&wake).expect("encode wake delivery")
    }

    fn wake_row() -> WakeDeliveryRow {
        WakeDeliveryRow {
            delivery_id: format!("wake:v1:sha256:{}", "a".repeat(64)),
            state_label: "enqueuing".to_string(),
            claim_token: Some("claim".to_string()),
            attempts: 2,
            first_attempt_ms: Some(11),
            next_attempt_at_ms: 12,
            expires_at_ms: 13,
            discard_reason_label: None,
            delivery_json: wake_delivery_json(),
        }
    }

    #[test]
    fn a_populated_wake_delivery_row_projects() {
        let row = wake_row();
        let delivery_id = row.delivery_id.clone();
        let delivery = row
            .project(lash_core_execution::FleetFormat::current())
            .expect("a well-formed row projects");
        assert_eq!(delivery.delivery_id, delivery_id);
        assert_eq!(delivery.state(), WakeDeliveryState::Enqueuing);
        assert_eq!(delivery.claim_token().expect("claim token"), "claim");
        assert_eq!(delivery.attempts, 2);
        assert_eq!(delivery.first_attempt_ms, Some(11));
        assert_eq!(delivery.next_attempt_at_ms, 12);
        assert_eq!(delivery.expires_at_ms, 13);
        assert_eq!(delivery.disposition.discard_reason(), None);
        assert_eq!(delivery.wake.sequence, 4);
        assert_eq!(delivery.wake.target_session_id, "session");
    }

    #[test]
    fn a_version_absent_v2_wake_delivery_is_refused() {
        let mut payload: serde_json::Value =
            serde_json::from_str(&wake_delivery_json()).expect("wake delivery JSON");
        payload
            .as_object_mut()
            .expect("wake delivery object")
            .remove("version");
        let error = WakeDeliveryRow {
            delivery_json: serde_json::to_string(&payload).expect("wake delivery JSON"),
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("a pre-version wake delivery row must be refused");

        assert!(matches!(
            error,
            PluginError::ProcessWakeDeliveryFormatVersionMismatch { expected, found }
                if expected == PROCESS_WAKE_DELIVERY_FORMAT_VERSION && found == 2
        ));
    }

    /// FIG-3607: a version-3 delivery named its process by a reusable name and
    /// an incarnation; a minted id names the whole lifetime under version 4.
    #[test]
    fn the_immediate_predecessor_wake_delivery_version_is_refused() {
        let mut payload: serde_json::Value =
            serde_json::from_str(&wake_delivery_json()).expect("wake delivery JSON");
        payload["version"] = serde_json::json!(3);
        let error = WakeDeliveryRow {
            delivery_json: serde_json::to_string(&payload).expect("wake delivery JSON"),
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("a version-3 wake delivery row must be refused");

        assert!(matches!(
            error,
            PluginError::ProcessWakeDeliveryFormatVersionMismatch { expected, found }
                if expected == PROCESS_WAKE_DELIVERY_FORMAT_VERSION && found == 3
        ));
        assert_eq!(
            3 + 1,
            PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            "wake delivery predecessor adjacency pin"
        );
    }

    #[test]
    fn a_future_wake_delivery_version_is_refused_with_expected_and_found() {
        let mut payload: serde_json::Value =
            serde_json::from_str(&wake_delivery_json()).expect("wake delivery JSON");
        payload["version"] = serde_json::json!(5);
        let error = WakeDeliveryRow {
            delivery_json: serde_json::to_string(&payload).expect("future wake delivery JSON"),
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("a future wake-delivery format must be refused");

        assert!(
            matches!(
                &error,
                PluginError::ProcessWakeDeliveryFormatVersionMismatch { expected, found }
                    if *expected == PROCESS_WAKE_DELIVERY_FORMAT_VERSION && *found == 5
            ),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn a_wake_delivery_missing_event_type_is_refused() {
        let mut payload: serde_json::Value =
            serde_json::from_str(&wake_delivery_json()).expect("wake delivery JSON");
        payload
            .as_object_mut()
            .expect("wake delivery object")
            .remove("event_type");
        let error = WakeDeliveryRow {
            delivery_json: serde_json::to_string(&payload).expect("wake delivery JSON"),
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("a wake delivery without event_type must be refused");

        assert!(
            matches!(&error, PluginError::Session(message)
                if message.starts_with("failed to decode process registry row: ")
                    && message.contains("missing field `event_type`")),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn a_discarded_wake_delivery_row_projects_its_reason() {
        let delivery = WakeDeliveryRow {
            state_label: "discarded".to_string(),
            discard_reason_label: Some("retargeted".to_string()),
            claim_token: None,
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect("a discarded row projects");
        assert_eq!(delivery.state(), WakeDeliveryState::Discarded);
        assert_eq!(
            delivery.disposition,
            WakeDeliveryDisposition::Discarded {
                reason: WakeDiscardReason::Retargeted
            }
        );
    }

    #[test]
    fn a_discarded_wake_delivery_without_a_reason_projects_as_unattributed() {
        let delivery = WakeDeliveryRow {
            state_label: "discarded".to_string(),
            discard_reason_label: None,
            claim_token: None,
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect("a deliberately-valid reasonless discard projects");

        assert_eq!(
            delivery.disposition,
            WakeDeliveryDisposition::DiscardedUnattributed
        );
        assert_eq!(delivery.state(), WakeDeliveryState::Discarded);
    }

    #[test]
    fn an_enqueuing_wake_delivery_without_a_claim_token_is_refused() {
        let delivery_id = format!("wake:v1:sha256:{}", "a".repeat(64));
        let error = WakeDeliveryRow {
            delivery_id: delivery_id.clone(),
            claim_token: None,
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("an enqueuing delivery without its claim token must be refused");

        assert!(
            matches!(&error, PluginError::Session(message)
            if message == &format!(
                "wake delivery `{delivery_id}` is enqueuing without a claim token"
            )),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn a_corrupt_wake_payload_reports_the_registry_decode_vocabulary() {
        let error = WakeDeliveryRow {
            delivery_json: "{".to_string(),
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("a corrupt payload is a decode failure");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message.starts_with("failed to decode process registry row: ")),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn an_unknown_state_label_outranks_a_corrupt_payload() {
        let error = WakeDeliveryRow {
            state_label: "settled".to_string(),
            delivery_json: "{".to_string(),
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("the state label is parsed before the payload is decoded");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message == "wake delivery `wake:v1:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa` has unknown state `settled`"),
            "unexpected refusal: {error}"
        );
    }
}
