mod tests {
    use lash_core_execution::SessionId;
    use lash_core_execution::plugin::PluginError;
    use lash_core_execution::runtime::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
    use lash_core_execution::runtime::process::registry_transitions::*;

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
    fn an_unknown_discard_reason_label_is_refused() {
        let error = wake_discard_reason_from_label("delivery", Some("bored"))
            .expect_err("an unrecognised reason is never defaulted");
        assert!(
            matches!(&error, PluginError::Session(message)
                if message == "wake delivery `delivery` has unknown discard reason `bored`"),
            "unexpected refusal: {error}"
        );
    }

    fn wake() -> lash_core_execution::runtime::ProcessWakeDelivery {
        lash_core_execution::runtime::ProcessWakeDelivery {
            version: PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
            target_session_id: SessionId::from("session"),
            process_id: lash_core_execution::ProcessId::fixture("process"),
            sequence: 4,
            event_type: "process.wake".to_string(),
            process_caused_by: None,
            authority: lash_core_execution::QueuedWorkAuthority::default(),
            input: "wake".to_string(),
            created_at_ms: 10,
            trace_cause: Default::default(),
        }
    }

    fn wake_delivery_json() -> String {
        serde_json::to_string(&wake()).expect("encode wake delivery")
    }

    fn wake_row() -> WakeDeliveryRow {
        WakeDeliveryRow {
            delivery_id: wake().wake_id().into_inner(),
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

    /// The row's key is the wake's identity, and the wake states it only
    /// through what it is computed from: a row keyed by any other id is
    /// refused, never projected as that other delivery.
    #[test]
    fn a_wake_delivery_row_keyed_by_another_wakes_id_is_refused() {
        let error = WakeDeliveryRow {
            delivery_id: format!("wake:v1:blake3:{}", "a".repeat(64)),
            ..wake_row()
        }
        .project(lash_core_execution::FleetFormat::current())
        .expect_err("a row whose key is not its wake's identity is refused");
        assert!(
            matches!(
                &error,
                PluginError::WakeDeliveryIdentityMismatch { delivery_id, wake_id }
                    if *delivery_id == format!("wake:v1:blake3:{}", "a".repeat(64))
                        && *wake_id == wake().wake_id()
            ),
            "unexpected refusal: {error}"
        );
    }

    /// A stored wake row requires its version: no pre-1.0 default reads an
    /// unstamped row as some older format (FIG-4818).
    #[test]
    fn a_version_absent_wake_delivery_is_refused() {
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

        assert!(
            matches!(&error, PluginError::Session(message)
                if message.starts_with("failed to decode process registry row: missing field `version`")),
            "unexpected refusal: {error}"
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
    fn a_wake_delivery_row_outside_its_lifecycle_is_refused() {
        let reason = Some("retargeted".to_string());
        let token = Some("claim".to_string());
        for (state, claim_token, discard_reason_label, refusal) in [
            (
                "discarded",
                None,
                None,
                "discarded without a discard reason",
            ),
            (
                "discarded",
                token.clone(),
                reason.clone(),
                "discarded with a claim token",
            ),
            ("pending", token.clone(), None, "pending with a claim token"),
            (
                "enqueued",
                token.clone(),
                None,
                "enqueued with a claim token",
            ),
            (
                "pending",
                None,
                reason.clone(),
                "pending with a discard reason",
            ),
            (
                "enqueued",
                None,
                reason.clone(),
                "enqueued with a discard reason",
            ),
            (
                "enqueuing",
                token,
                reason,
                "enqueuing with a discard reason",
            ),
        ] {
            let error = WakeDeliveryRow {
                state_label: state.to_string(),
                discard_reason_label,
                claim_token,
                ..wake_row()
            }
            .project(lash_core_execution::FleetFormat::current())
            .expect_err("a row outside the wake-delivery lifecycle is refused");
            assert!(error.to_string().contains(refusal), "{state}: {error}");
        }
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
}
