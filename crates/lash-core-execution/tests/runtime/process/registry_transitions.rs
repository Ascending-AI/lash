mod tests {
    use lash_core_execution::SessionId;
    use lash_core_execution::plugin::PluginError;
    use lash_core_execution::runtime::process::registry_transitions::*;
    use lash_core_execution::runtime::{
        PROCESS_LEASE_SCHEMA_VERSION, PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
        WakeDeliveryDisposition, WakeDeliveryState, WakeDiscardReason,
    };
    use lash_core_execution::{LeaseOwnerIdentity, ProcessLease, ProcessStatus};

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

    fn owner(owner_id: &str, incarnation_id: &str) -> LeaseOwnerIdentity {
        LeaseOwnerIdentity {
            owner_id: owner_id.to_string(),
            incarnation_id: incarnation_id.to_string(),
        }
    }

    fn lease(now_ms: u64, ttl_ms: u64, fencing_token: u64) -> ProcessLease {
        acquired_process_lease(
            &lash_core_execution::ProcessId::fixture("process"),
            &owner("worker", "incarnation"),
            fencing_token,
            now_ms,
            ttl_ms,
            lash_core_execution::FleetFormat::current(),
        )
    }

    // --- A1 / A7: the fence -------------------------------------------------

    #[test]
    fn fence_holds_for_the_exact_stored_lease_before_expiry() {
        let stored = lease(1_000, 5_000, 3);
        assert!(process_lease_still_holds(
            Some(&stored),
            &stored.clone(),
            5_999
        ));
    }

    #[test]
    fn fence_fails_on_a_different_lease_token() {
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            lease_token: "other".to_string(),
            ..stored.clone()
        };
        assert!(!process_lease_still_holds(Some(&stored), &claimed, 2_000));
    }

    #[test]
    fn fence_fails_on_a_different_owner_incarnation() {
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            owner: owner("worker", "restarted"),
            ..stored.clone()
        };
        assert!(
            !process_lease_still_holds(Some(&stored), &claimed, 2_000),
            "a restarted incarnation of the same owner id is not the holder"
        );
    }

    #[test]
    fn fence_fails_on_a_different_owner_id() {
        // `same_incarnation` compares both halves of the identity; this pins
        // the owner-id half, which the incarnation-variation law above cannot.
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            owner: owner("other-worker", "incarnation"),
            ..stored.clone()
        };
        assert!(
            !process_lease_still_holds(Some(&stored), &claimed, 2_000),
            "a different owner id with the stored token and incarnation label is not the holder"
        );
    }

    #[test]
    fn fence_fails_on_a_different_fencing_token() {
        let stored = lease(1_000, 5_000, 3);
        let claimed = ProcessLease {
            fencing_token: 4,
            ..stored.clone()
        };
        assert!(!process_lease_still_holds(Some(&stored), &claimed, 2_000));
    }

    #[test]
    fn fence_fails_at_the_exact_expiry_instant() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(stored.expires_at_epoch_ms, 6_000);
        assert!(process_lease_still_holds(
            Some(&stored),
            &stored.clone(),
            5_999
        ));
        assert!(
            !process_lease_still_holds(Some(&stored), &stored.clone(), 6_000),
            "a lease expiring at `now` is expired, not live"
        );
    }

    #[test]
    fn fence_fails_when_no_lease_is_stored() {
        let claimed = lease(1_000, 5_000, 3);
        assert!(!process_lease_still_holds(None, &claimed, 2_000));
    }

    #[test]
    fn authorization_refuses_a_lease_for_another_process() {
        let stored = lease(1_000, 5_000, 3);
        let error = authorize_process_lease_write(
            &lash_core_execution::ProcessId::fixture("other-process"),
            &stored,
            Some(&stored),
            2_000,
        )
        .expect_err("a lease is only authority over its own process");
        assert!(
            matches!(
                &error,
                PluginError::ProcessLeaseSuperseded { process_id } if process_id == lash_core_execution::ProcessId::fixture("other-process")
            ),
            "unexpected refusal: {error}"
        );
    }

    #[test]
    fn authorization_succeeds_exactly_when_the_fence_holds() {
        let stored = lease(1_000, 5_000, 3);
        assert!(
            authorize_process_lease_write(
                &lash_core_execution::ProcessId::fixture("process"),
                &stored,
                Some(&stored),
                2_000
            )
            .is_ok(),
            "the holder's own live lease authorizes its write"
        );
        let error = authorize_process_lease_write(
            &lash_core_execution::ProcessId::fixture("process"),
            &stored,
            None,
            2_000,
        )
        .expect_err("a released lease authorizes nothing");
        assert!(
            matches!(&error, PluginError::ProcessLeaseSuperseded { process_id } if process_id == lash_core_execution::ProcessId::fixture("process")),
            "unexpected refusal: {error}"
        );
        assert!(
            authorize_process_lease_write(
                &lash_core_execution::ProcessId::fixture("process"),
                &stored,
                Some(&stored),
                6_000
            )
            .is_err(),
            "an expired lease authorizes nothing"
        );
    }

    // --- A2 / A3 / A4: claim and reclaim -----------------------------------

    /// Compact rendering of a claim decision, so the arm laws read as equalities.
    fn arm(decision: &ProcessLeaseClaimDecision) -> String {
        match decision {
            ProcessLeaseClaimDecision::ExtendHeldLease { lease } => format!(
                "extend(token={}, fence={}, expires={})",
                lease.lease_token, lease.fencing_token, lease.expires_at_epoch_ms
            ),
            ProcessLeaseClaimDecision::ReportBusy { holder } => {
                format!("busy(token={})", holder.lease_token)
            }
            ProcessLeaseClaimDecision::AcquireOnRetainedFence => "acquire(retained)".to_string(),
        }
    }

    /// The same rendering for a reclaim decision.
    fn reclaim_arm(decision: &ProcessLeaseReclaimDecision) -> String {
        match decision {
            ProcessLeaseReclaimDecision::ReportBusy { holder } => {
                format!("busy(token={})", holder.lease_token)
            }
            ProcessLeaseReclaimDecision::AcquireOnRetainedFence => "acquire(retained)".to_string(),
            ProcessLeaseReclaimDecision::AcquireOnObservedFence { fencing_token } => {
                format!("acquire(observed={fencing_token})")
            }
        }
    }

    #[test]
    fn claim_extends_its_own_live_lease_without_rotating_the_fence() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &stored.owner,
                2_000,
                5_000
            )),
            format!(
                "extend(token={}, fence=3, expires=7000)",
                stored.lease_token
            ),
            "a re-entering incarnation keeps its token and fencing token"
        );
    }

    #[test]
    fn claim_reports_busy_for_another_incarnation_on_a_live_lease() {
        let stored = lease(1_000, 5_000, 3);
        let busy = format!("busy(token={})", stored.lease_token);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("worker", "restarted"),
                2_000,
                5_000
            )),
            busy,
            "a restarted incarnation of the same owner id must not steal the lease"
        );
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                2_000,
                5_000
            )),
            busy
        );
    }

    #[test]
    fn claim_on_an_absent_lease_acquires_on_the_retained_fence() {
        assert_eq!(
            arm(&decide_process_lease_claim(
                None,
                &owner("worker", "incarnation"),
                2_000,
                5_000
            )),
            "acquire(retained)"
        );
    }

    #[test]
    fn claim_on_an_expired_lease_still_acquires_on_the_retained_fence() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                6_000,
                5_000
            )),
            "acquire(retained)",
            "claim re-reads the retained column; only reclaim uses the observed lease"
        );
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &stored.owner,
                6_000,
                5_000
            )),
            "acquire(retained)",
            "an expired lease is not extended, even by its own incarnation"
        );
    }

    #[test]
    fn reclaim_on_an_absent_lease_acquires_on_the_retained_fence() {
        assert_eq!(
            reclaim_arm(&decide_process_lease_reclaim(None, 2_000).expect("decide reclaim")),
            "acquire(retained)"
        );
    }

    #[test]
    fn reclaim_on_an_expired_lease_acquires_on_the_observed_successor() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 6_000).expect("decide reclaim"),
            ),
            "acquire(observed=4)"
        );
    }

    #[test]
    fn reclaim_reports_busy_on_a_live_lease_whoever_holds_it() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 2_000).expect("decide reclaim"),
            ),
            format!("busy(token={})", stored.lease_token),
            "reclaim has no same-incarnation extend arm"
        );
    }

    #[test]
    fn the_expiry_boundary_is_the_same_for_claim_and_reclaim() {
        let stored = lease(1_000, 5_000, 3);
        assert_eq!(stored.expires_at_epoch_ms, 6_000);
        let busy = format!("busy(token={})", stored.lease_token);
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                5_999,
                5_000
            )),
            busy
        );
        assert_eq!(
            arm(&decide_process_lease_claim(
                Some(&stored),
                &owner("rival", "rival"),
                6_000,
                5_000
            )),
            "acquire(retained)"
        );
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 5_999).expect("decide reclaim"),
            ),
            busy
        );
        assert_eq!(
            reclaim_arm(
                &decide_process_lease_reclaim(Some(&stored), 6_000).expect("decide reclaim"),
            ),
            "acquire(observed=4)"
        );
    }

    #[test]
    fn fencing_token_succession_starts_at_one_and_refuses_overflow() {
        assert_eq!(next_process_lease_fencing_token(0).expect("advance"), 1);
        assert_eq!(next_process_lease_fencing_token(41).expect("advance"), 42);
        assert!(matches!(
            next_process_lease_fencing_token(i64::MAX as u64),
            Err(PluginError::MonotonicCounterOverflow {
                ref counter,
                current,
            }) if counter == "process_lease_fencing_token" && current == i64::MAX as u64
        ));
    }

    // --- A5: lease-row projection ------------------------------------------

    fn populated_row() -> ProcessLeaseRow {
        ProcessLeaseRow {
            owner_id: Some("worker".to_string()),
            incarnation_id: Some("incarnation".to_string()),
            lease_token: Some("token".to_string()),
            fencing_token: 7,
            claimed_at_ms: 1_000,
            expires_at_ms: 6_000,
        }
    }

    #[test]
    fn a_populated_lease_row_projects() {
        let lease = populated_row()
            .project(
                &lash_core_execution::ProcessId::fixture("process"),
                lash_core_execution::FleetFormat::current(),
            )
            .expect("a row with an owner and a token records a holder");
        assert_eq!(lease.schema_version, PROCESS_LEASE_SCHEMA_VERSION);
        assert_eq!(
            lease.process_id,
            lash_core_execution::ProcessId::fixture("process")
        );
        assert_eq!(lease.owner, owner("worker", "incarnation"));
        assert_eq!(lease.lease_token, "token");
        assert_eq!(lease.fencing_token, 7);
        assert_eq!(lease.claimed_at_epoch_ms, 1_000);
        assert_eq!(lease.expires_at_epoch_ms, 6_000);
    }

    #[test]
    fn a_null_owner_id_records_no_holder() {
        assert!(
            ProcessLeaseRow {
                owner_id: None,
                ..populated_row()
            }
            .project(
                &lash_core_execution::ProcessId::fixture("process"),
                lash_core_execution::FleetFormat::current()
            )
            .is_none()
        );
    }

    #[test]
    fn a_null_lease_token_records_no_holder() {
        assert!(
            ProcessLeaseRow {
                lease_token: None,
                ..populated_row()
            }
            .project(
                &lash_core_execution::ProcessId::fixture("process"),
                lash_core_execution::FleetFormat::current()
            )
            .is_none()
        );
    }

    #[test]
    fn a_null_incarnation_id_defaults_to_the_owner_id() {
        let lease = ProcessLeaseRow {
            incarnation_id: None,
            ..populated_row()
        }
        .project(
            &lash_core_execution::ProcessId::fixture("process"),
            lash_core_execution::FleetFormat::current(),
        )
        .expect("pre-incarnation rows still record a holder");
        assert_eq!(lease.owner, owner("worker", "worker"));
    }

    #[test]
    fn a_released_row_records_no_holder_but_retains_its_fence() {
        let released = ProcessLeaseRow {
            owner_id: None,
            incarnation_id: None,
            lease_token: None,
            fencing_token: 9,
            claimed_at_ms: 0,
            expires_at_ms: 0,
        };
        let retained = released.fencing_token as u64;
        assert!(
            released
                .project(
                    &lash_core_execution::ProcessId::fixture("process"),
                    lash_core_execution::FleetFormat::current()
                )
                .is_none(),
            "a released lease is not a holder"
        );
        assert_eq!(
            next_process_lease_fencing_token(retained).expect("advance retained fence"),
            10,
            "the retained counter still fences the next holder"
        );
    }

    // --- A6: lease minting -------------------------------------------------

    #[test]
    fn the_lease_token_preimage_is_pinned() {
        let minted = acquired_process_lease(
            &lash_core_execution::ProcessId::fixture("process-a"),
            &owner("owner-a", "incarnation-a"),
            5,
            1_700_000_000_000,
            30_000,
            lash_core_execution::FleetFormat::current(),
        );
        // blake3("p_c5546c16060677e5a56c42a3a5c9a6fc:owner-a:incarnation-a:1700000000000:5").
        // Durable value: changing this literal changes every backend's lease
        // identity.
        assert_eq!(
            minted.lease_token,
            "a0999326951122cd5d5ab107620a47954584ed70fb1ae0a603684451879ef1e6"
        );
        assert_eq!(
            minted.process_id,
            lash_core_execution::ProcessId::fixture("process-a")
        );
        assert_eq!(minted.fencing_token, 5);
        assert_eq!(minted.claimed_at_epoch_ms, 1_700_000_000_000);
        assert_eq!(minted.expires_at_epoch_ms, 1_700_000_030_000);
        assert_eq!(minted.schema_version, PROCESS_LEASE_SCHEMA_VERSION);
    }

    #[test]
    fn the_lease_token_preimage_is_field_ordered() {
        let straight = acquired_process_lease(
            &lash_core_execution::ProcessId::fixture("p"),
            &owner("a", "b"),
            1,
            10,
            10,
            lash_core_execution::FleetFormat::current(),
        );
        let swapped = acquired_process_lease(
            &lash_core_execution::ProcessId::fixture("p"),
            &owner("b", "a"),
            1,
            10,
            10,
            lash_core_execution::FleetFormat::current(),
        );
        assert_ne!(
            straight.lease_token, swapped.lease_token,
            "owner id and incarnation id occupy distinct preimage positions"
        );
        for (left, right) in [
            (
                acquired_process_lease(
                    &lash_core_execution::ProcessId::fixture("p"),
                    &owner("a", "b"),
                    2,
                    10,
                    10,
                    lash_core_execution::FleetFormat::current(),
                )
                .lease_token,
                straight.lease_token.clone(),
            ),
            (
                acquired_process_lease(
                    &lash_core_execution::ProcessId::fixture("p"),
                    &owner("a", "b"),
                    1,
                    11,
                    10,
                    lash_core_execution::FleetFormat::current(),
                )
                .lease_token,
                straight.lease_token.clone(),
            ),
            (
                acquired_process_lease(
                    &lash_core_execution::ProcessId::fixture("q"),
                    &owner("a", "b"),
                    1,
                    10,
                    10,
                    lash_core_execution::FleetFormat::current(),
                )
                .lease_token,
                straight.lease_token.clone(),
            ),
        ] {
            assert_ne!(left, right, "every preimage field must move the token");
        }
    }

    #[test]
    fn the_minted_expiry_saturates() {
        let minted = acquired_process_lease(
            &lash_core_execution::ProcessId::fixture("p"),
            &owner("a", "b"),
            1,
            u64::MAX - 1,
            10,
            lash_core_execution::FleetFormat::current(),
        );
        assert_eq!(minted.expires_at_epoch_ms, u64::MAX);
    }

    // --- B1 / B2: terminal and tombstone classification -------------------

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
