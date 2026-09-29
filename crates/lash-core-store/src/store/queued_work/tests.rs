use super::*;
use crate::{SessionId, TurnId};
use proptest::{
    collection::vec,
    prelude::*,
    test_runner::{Config, RngSeed, TestRunner},
};

// The admission-law tests below assert on selection sizes alone; these shadows
// keep them reading that way while the real functions also carry the
// refusal. Refusal coverage lives in `each_refusal_names_the_scenario_that_produces_it`.
fn select_turn_work_prefix(
    candidates: &[TurnLaneCandidate],
    boundary: AdmissionBoundary,
    policy: &TurnLaneAdmissionPolicy,
    now_epoch_ms: u64,
) -> Result<usize, StoreError> {
    super::select_turn_work_prefix(candidates, boundary, policy, now_epoch_ms).map(|prefix| {
        match prefix {
            TurnWorkPrefix::Selected { len } => len,
            TurnWorkPrefix::Refused { .. } => 0,
        }
    })
}

fn select_turn_work_indices(
    candidates: &[TurnLaneCandidate],
    boundary: AdmissionBoundary,
    policy: &TurnLaneAdmissionPolicy,
    now_epoch_ms: u64,
) -> Result<Vec<usize>, StoreError> {
    super::select_turn_work_indices(candidates, boundary, policy, now_epoch_ms).map(|selection| {
        match selection {
            TurnWorkSelection::Selected { indices } => indices,
            TurnWorkSelection::Refused { .. } => Vec::new(),
        }
    })
}

fn candidate(enqueue_seq: u64, merge_key: Option<&str>) -> TurnLaneCandidate {
    TurnLaneCandidate {
        batch_id: format!("qwb-{enqueue_seq}").into(),
        enqueue_seq,
        config_patch_command: false,
        delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
        kind: QueuedWorkKind::Turn,
        authority: QueuedWorkAuthority::new("principal"),
        merge_key: merge_key.map(str::to_string),
        enqueued_at_ms: 900,
        turn_causes: vec![wake_cause(enqueue_seq, "wake")],
    }
}

/// The turn cause a durable process wake renders into the admission's
/// model-visible prompt, the only queued work the token bound measures.
/// Identifiers stay short (the process id is a minted-form fixture id, the
/// rest single words) so one default row renders under the tiny windows some
/// admission laws below use, while several rows together do not.
fn wake_cause(sequence: u64, text: &str) -> TurnCause {
    TurnCause {
        id: format!("w{sequence}"),
        event_type: "wake".to_string(),
        origin: crate::MessageOrigin::Process {
            process_id: crate::ProcessId::fixture("p"),
            event_type: "wake".to_string(),
            sequence,
            wake_id: Some(format!("w{sequence}")),
            caused_by: None,
        },
        text: text.to_string(),
    }
}

fn rendered_candidate_strategy() -> impl Strategy<Value = TurnLaneCandidate> {
    let merge_key = prop_oneof![
        Just(None),
        Just(Some("wake".to_string())),
        Just(Some("other".to_string())),
    ];
    let kind = prop_oneof![Just(QueuedWorkKind::Turn), Just(QueuedWorkKind::Control),];
    let delivery_policy = prop_oneof![
        Just(DeliveryPolicy::EarliestSafeBoundary),
        Just(DeliveryPolicy::AfterCurrentTurnCommit),
    ];
    let authority = prop_oneof![
        Just(QueuedWorkAuthority::default()),
        Just(QueuedWorkAuthority::new("principal-a")),
        Just(QueuedWorkAuthority::new("principal-b").with_elevation("root")),
    ];
    let turn_causes = vec((0usize..=128, 0u8..3), 0..=4).prop_map(|causes| {
        causes
            .into_iter()
            .enumerate()
            .map(|(index, (text_len, origin_kind))| TurnCause {
                id: format!("cause-{index}"),
                event_type: format!("event-{origin_kind}"),
                origin: match origin_kind {
                    0 => crate::MessageOrigin::Plugin {
                        plugin_id: format!("plugin-{index}"),
                        transient: index % 2 == 0,
                    },
                    1 => crate::MessageOrigin::Process {
                        process_id: crate::process_id_for_test(&format!("process-{index}")),
                        event_type: "wake".to_string(),
                        sequence: index as u64,
                        wake_id: Some(format!("wake-{index}")),
                        caused_by: None,
                    },
                    _ => crate::MessageOrigin::TurnInput {
                        turn_id: TurnId::from(format!("turn-{index}")),
                        input_id: (index % 2 == 0)
                            .then(|| crate::InputId::new(format!("input-{index}"))),
                    },
                },
                text: "x".repeat(text_len),
            })
            .collect::<Vec<_>>()
    });

    (
        any::<u64>(),
        merge_key,
        kind,
        delivery_policy,
        authority,
        turn_causes,
    )
        .prop_map(
            |(enqueue_seq, merge_key, kind, delivery_policy, authority, turn_causes)| {
                TurnLaneCandidate {
                    batch_id: format!("qwb-{enqueue_seq}").into(),
                    enqueue_seq,
                    config_patch_command: false,
                    delivery_policy,
                    kind,
                    authority,
                    merge_key,
                    enqueued_at_ms: 900,
                    turn_causes,
                }
            },
        )
}

fn policy(max_context_tokens: usize, action_token_reserve: usize) -> TurnLaneAdmissionPolicy {
    TurnLaneAdmissionPolicy {
        max_context_tokens,
        action_token_reserve,
        max_rows: 64,
        max_pending_age_ms: 1_000,
        drain_policy: crate::default_queued_drain_policy(),
    }
}

/// Every refusal a host can be handed is pinned to the one scenario that
/// produces it. A drain that reports the wrong reason is how queued work
/// gets abandoned (FIG-1575), so these are asserted by name, not by
/// emptiness.
#[test]
fn each_refusal_names_the_scenario_that_produces_it() {
    let mut zero_row_policy = policy(1_000, 100);
    zero_row_policy.max_rows = 0;
    let mut command_head = candidate(1, None);
    command_head.kind = QueuedWorkKind::Control;
    let mut boundary_blocked = candidate(1, None);
    boundary_blocked.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;

    let cases: Vec<(&str, Vec<TurnLaneCandidate>, AdmissionBoundary, _, _)> = vec![
        (
            "a host policy that admits no rows",
            vec![candidate(1, None)],
            AdmissionBoundary::Idle,
            zero_row_policy,
            AdmissionRefusal::ZeroLimit,
        ),
        (
            "an exhausted queue",
            Vec::new(),
            AdmissionBoundary::Idle,
            policy(1_000, 100),
            AdmissionRefusal::Empty,
        ),
        (
            "a session command at the queue head",
            vec![command_head],
            AdmissionBoundary::Idle,
            policy(1_000, 100),
            AdmissionRefusal::CommandAtHead,
        ),
        (
            "a head that may not cross the active turn boundary",
            vec![boundary_blocked],
            AdmissionBoundary::ActiveTurnCheckpoint,
            policy(1_000, 100),
            AdmissionRefusal::DeliveryBoundaryBlocked,
        ),
    ];
    for (scenario, candidates, boundary, admission_policy, expected) in cases {
        let selection =
            super::select_turn_work_indices(&candidates, boundary, &admission_policy, 1_000)
                .expect("admission laws hold");
        assert_eq!(
            selection,
            TurnWorkSelection::Refused { reason: expected },
            "{scenario}"
        );
        let prefix =
            super::select_turn_work_prefix(&candidates, boundary, &admission_policy, 1_000)
                .expect("admission laws hold");
        assert_eq!(
            prefix,
            TurnWorkPrefix::Refused { reason: expected },
            "{scenario}"
        );
    }
}

/// The spellings travel into host logs and metrics labels, and they are the
/// same strings the admission-decision diagnostics have always emitted.
#[test]
fn refusal_spellings_are_stable() {
    let cases = [
        (AdmissionRefusal::ZeroLimit, "zero_limit"),
        (AdmissionRefusal::Empty, "empty"),
        (AdmissionRefusal::CommandAtHead, "command_at_head"),
        (
            AdmissionRefusal::DeliveryBoundaryBlocked,
            "delivery_boundary_blocked",
        ),
        (AdmissionRefusal::HeadWithheld, "head_withheld"),
        (AdmissionRefusal::AdmissionRaceLost, "admission_race_lost"),
    ];
    for (refusal, expected) in cases {
        assert_eq!(refusal.as_str(), expected);
    }
}

#[test]
fn absent_merge_key_never_merges() {
    let candidates = vec![candidate(1, None), candidate(2, None)];
    assert_eq!(
        select_turn_work_prefix(
            &candidates,
            AdmissionBoundary::Idle,
            &policy(1_000, 100),
            1_000,
        )
        .unwrap(),
        1
    );
}

#[test]
fn matching_key_groups_prefix_up_to_row_bound() {
    let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
    let mut admission_policy = policy(1_000, 100);
    admission_policy.max_rows = 1;
    assert_eq!(
        select_turn_work_prefix(
            &candidates,
            AdmissionBoundary::Idle,
            &admission_policy,
            1_000,
        )
        .unwrap(),
        1
    );
}

#[test]
fn authority_and_elevation_are_independent_compatibility_gates() {
    let first = candidate(1, Some("wake"));
    let mut different_principal = candidate(2, Some("wake"));
    different_principal.authority = QueuedWorkAuthority::new("other");
    let mut different_elevation = candidate(2, Some("wake"));
    different_elevation.authority = QueuedWorkAuthority::new("principal").with_elevation("root");
    for candidates in [
        vec![first.clone(), different_principal],
        vec![first.clone(), different_elevation],
    ] {
        assert_eq!(
            select_turn_work_prefix(
                &candidates,
                AdmissionBoundary::Idle,
                &policy(1_000, 100),
                1_000
            )
            .unwrap(),
            1
        );
    }
}

#[test]
fn control_kind_is_a_command_barrier() {
    let mut first = candidate(1, Some("wake"));
    first.kind = QueuedWorkKind::Control;
    // Kind now states the family completely; Control cannot masquerade as
    // turn work by carrying an independent work_class value.
    assert!(!first.kind.is_batchable());
    let candidates = vec![first, candidate(2, Some("wake"))];
    assert_eq!(select_leading_session_command(&candidates), 1);
    let selection = super::select_turn_work_prefix(
        &candidates,
        AdmissionBoundary::Idle,
        &policy(1_000, 100),
        1_000,
    )
    .unwrap();
    assert_eq!(
        selection,
        TurnWorkPrefix::Refused {
            reason: AdmissionRefusal::CommandAtHead
        }
    );
}

#[test]
fn merge_key_delivery_and_work_class_mismatches_break_prefix() {
    let first = candidate(1, Some("a"));
    let mut different_delivery = candidate(2, Some("a"));
    different_delivery.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;
    let mut command = candidate(2, Some("a"));
    command.kind = QueuedWorkKind::Control;
    for candidates in [
        vec![first.clone(), candidate(2, Some("b"))],
        vec![first.clone(), different_delivery],
        vec![first.clone(), command],
    ] {
        assert_eq!(
            select_turn_work_prefix(
                &candidates,
                AdmissionBoundary::Idle,
                &policy(1_000, 100),
                1_000
            )
            .unwrap(),
            1
        );
    }
}

#[test]
fn all_mode_admits_the_whole_compatible_prefix_without_token_arithmetic() {
    let candidates = vec![
        candidate(1, Some("wake")),
        candidate(2, Some("wake")),
        candidate(3, Some("wake")),
    ];
    // One row fits this deliberately tiny window; the three together do not,
    // as the default token-bounded drain shows.
    let mut admission_policy = policy(131, 30);
    assert_ne!(
        select_turn_work_indices(
            &candidates,
            AdmissionBoundary::Idle,
            &admission_policy,
            1_000,
        )
        .unwrap(),
        vec![0, 1, 2],
        "the three rows must render past the window"
    );
    admission_policy.drain_policy =
        std::sync::Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
    // `All` is a host statement that the provider is the authority on what
    // fits, so Lash coalesces every compatible row anyway.
    assert_eq!(
        select_turn_work_indices(
            &candidates,
            AdmissionBoundary::Idle,
            &admission_policy,
            1_000,
        )
        .unwrap(),
        vec![0, 1, 2]
    );
}

#[test]
fn a_custom_policy_selection_is_clamped_to_the_legal_prefix() {
    #[derive(Debug)]
    struct GreedyPolicy;
    impl crate::QueuedDrainPolicy for GreedyPolicy {
        fn name(&self) -> &str {
            "test_greedy"
        }

        fn select_drain(
            &self,
            request: &crate::QueuedDrainRequest<'_>,
        ) -> crate::QueuedDrainSelection {
            // Every offered candidate carries a projection and a budget.
            assert!(
                request
                    .candidates()
                    .iter()
                    .all(|candidate| candidate.projected_tokens > 0)
            );
            assert_eq!(request.max_context_tokens(), 1_000);
            crate::QueuedDrainSelection::leading(usize::MAX)
        }
    }

    let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
    let mut admission_policy = policy(1_000, 100);
    admission_policy.drain_policy = std::sync::Arc::new(GreedyPolicy);
    assert_eq!(
        select_turn_work_indices(
            &candidates,
            AdmissionBoundary::Idle,
            &admission_policy,
            1_000,
        )
        .unwrap(),
        vec![0, 1]
    );

    #[derive(Debug)]
    struct EmptyPolicy;
    impl crate::QueuedDrainPolicy for EmptyPolicy {
        fn name(&self) -> &str {
            "test_empty"
        }

        fn select_drain(
            &self,
            _request: &crate::QueuedDrainRequest<'_>,
        ) -> crate::QueuedDrainSelection {
            crate::QueuedDrainSelection::leading(0)
        }
    }

    let mut empty_policy = policy(1_000, 100);
    empty_policy.drain_policy = std::sync::Arc::new(EmptyPolicy);
    // A policy cannot starve its own queue: the head always drains.
    assert_eq!(
        select_turn_work_indices(&candidates, AdmissionBoundary::Idle, &empty_policy, 1_000,)
            .unwrap(),
        vec![0]
    );
}

#[test]
fn an_oversized_non_head_row_clamps_the_drain_instead_of_failing_it() {
    let mut first = candidate(1, Some("wake"));
    first.turn_causes = vec![wake_cause(1, &"a".repeat(8))];
    let mut second = candidate(2, Some("wake"));
    second.turn_causes = vec![wake_cause(2, &"b".repeat(4_000))];
    let third = candidate(3, Some("wake"));
    let mut admission_policy = policy(1_000, 100);
    admission_policy.drain_policy =
        std::sync::Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
    // The fitting head still drains: the selection stops before the
    // oversized row rather than failing an admission that can make progress.
    assert_eq!(
        select_turn_work_indices(
            &[first.clone(), second.clone(), third],
            AdmissionBoundary::Idle,
            &admission_policy,
            1_000,
        )
        .unwrap(),
        vec![0]
    );
    // On the next wake the oversized row is the head, and it is refused
    // there by name rather than wedging the queue silently.
    let error = select_turn_work_indices(
        &[second, first],
        AdmissionBoundary::Idle,
        &admission_policy,
        1_000,
    )
    .expect_err("an oversized head row must be refused by name");
    match error {
        StoreError::QueuedWorkRowExceedsContextWindow {
            batch_id,
            batch_enqueue_seq,
            rendered_tokens,
            max_context_tokens,
        } => {
            assert_eq!(batch_id, "qwb-2");
            assert_eq!(batch_enqueue_seq, 2);
            assert!(rendered_tokens > max_context_tokens);
            assert_eq!(max_context_tokens, 1_000);
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn rendered_bound_is_monotonic_over_prefixes() {
    const SEED: u64 = 0x5eed_f101_4004_0002;
    let mut runner = TestRunner::new(Config {
        cases: 512,
        failure_persistence: None,
        rng_seed: RngSeed::Fixed(SEED),
        ..Config::default()
    });

    runner
        .run(&vec(rendered_candidate_strategy(), 1..=8), |candidates| {
            for prefix_len in 1..candidates.len() {
                let prefix_bound =
                    rendered_token_upper_bound(&candidates[..prefix_len]);
                let extended_bound =
                    rendered_token_upper_bound(&candidates[..=prefix_len]);
                prop_assert!(
                    prefix_bound <= extended_bound,
                    "seed={SEED:#x}, prefix_len={prefix_len}, prefix_bound={prefix_bound}, extended_bound={extended_bound}, candidates={candidates:#?}"
                );
            }
            Ok(())
        })
        .expect("rendered token bound must be monotonic over prefixes");
}

#[test]
fn oversized_for_reserve_but_fitting_context_is_attempted_alone() {
    let mut first = candidate(1, Some("wake"));
    first.turn_causes = vec![wake_cause(1, &"a".repeat(800))];
    assert_eq!(
        select_turn_work_prefix(
            &[first],
            AdmissionBoundary::Idle,
            &policy(1_000, 300),
            1_000
        )
        .unwrap(),
        1
    );
}

#[test]
fn row_that_cannot_fit_context_fails_loudly() {
    let mut first = candidate(7, Some("wake"));
    first.turn_causes = vec![wake_cause(7, &"a".repeat(1_001))];
    assert!(matches!(
        select_turn_work_prefix(
            &[first],
            AdmissionBoundary::Idle,
            &policy(1_000, 300),
            1_000
        ),
        Err(StoreError::QueuedWorkRowExceedsContextWindow {
            batch_enqueue_seq: 7,
            ..
        })
    ));
}

#[test]
fn active_turn_checkpoint_boundary_gates_on_delivery_policy() {
    let mut first = candidate(1, None);
    first.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;
    assert_eq!(
        select_turn_work_prefix(
            &[first],
            AdmissionBoundary::ActiveTurnCheckpoint,
            &policy(1_000, 100),
            1_000,
        )
        .unwrap(),
        0
    );
}

#[test]
fn leading_session_command_blocks_turn_work_admission() {
    let mut command = candidate(1, None);
    command.kind = QueuedWorkKind::Control;
    let candidates = vec![command, candidate(2, None)];
    assert_eq!(select_leading_session_command(&candidates), 1);
    assert_eq!(
        select_turn_work_prefix(
            &candidates,
            AdmissionBoundary::Idle,
            &policy(1_000, 100),
            1_000
        )
        .unwrap(),
        0
    );
}

#[test]
fn adjacent_config_commands_share_one_admission_but_not_other_commands() {
    let mut first = candidate(1, None);
    first.kind = QueuedWorkKind::Control;
    first.config_patch_command = true;
    let mut second = first.clone();
    second.batch_id = "qwb-2".into();
    second.enqueue_seq = 2;
    let mut refresh = second.clone();
    refresh.batch_id = "qwb-3".into();
    refresh.enqueue_seq = 3;
    refresh.config_patch_command = false;

    assert_eq!(select_leading_session_command(&[first, second, refresh]), 2);
}

#[test]
fn overdue_head_is_admitted_alone_at_admission_time() {
    let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
    assert_eq!(
        select_turn_work_prefix(
            &candidates,
            AdmissionBoundary::Idle,
            &policy(1_000, 100),
            2_000
        )
        .unwrap(),
        1
    );
}

#[test]
fn batch_id_includes_optional_nonce() {
    let plain = derive_batch_id(&SessionId::from("session"), Some("key"), 1_000, None);
    let nonced = derive_batch_id(&SessionId::from("session"), Some("key"), 1_000, Some(1));
    assert_ne!(plain, nonced);
    assert!(plain.starts_with("qwb:"));
}

#[test]
fn pending_session_ordering_drains_commands_first() {
    let key = |enqueued_at_ms, enqueue_seq| PendingWorkOrderingKey {
        enqueued_at_ms,
        enqueue_seq,
    };
    let precedes = |command, input| {
        PendingSessionWorkOrdering {
            session_command: command,
            turn_input: input,
        }
        .session_command_precedes_turn_input()
    };

    assert!(precedes(Some(key(10, 9)), Some(key(11, 1))));
    assert!(precedes(Some(key(11, 1)), Some(key(10, 9))));
    // Commands precede inputs regardless of timestamps or sequence.
    assert!(precedes(Some(key(10, 1)), Some(key(10, 2))));
    assert!(precedes(Some(key(10, 2)), Some(key(10, 1))));
    assert!(precedes(Some(key(10, 1)), Some(key(10, 1))));
    assert!(precedes(Some(key(10, 1)), None));
    assert!(!precedes(None, Some(key(10, 1))));
}
