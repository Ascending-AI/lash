use super::*;
use proptest::{
    collection::vec,
    prelude::*,
    test_runner::{Config, RngSeed, TestRunner},
};

// The claim-law tests below assert on selection sizes alone; these shadows
// keep them reading that way while the real functions also carry the
// refusal. Refusal coverage lives in `each_refusal_names_the_scenario_that_produces_it`.
fn select_turn_work_claim_prefix(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<usize, StoreError> {
    super::select_turn_work_claim_prefix(candidates, boundary, policy, now_epoch_ms)
        .map(|prefix| prefix.len)
}

fn select_exact_turn_work_claim_prefix(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<usize, StoreError> {
    super::select_exact_turn_work_claim_prefix(candidates, boundary, policy, now_epoch_ms)
        .map(|prefix| prefix.len)
}

fn select_turn_work_claim_indices(
    candidates: &[ClaimCandidate],
    boundary: QueuedWorkClaimBoundary,
    policy: &QueuedWorkClaimPolicy,
    now_epoch_ms: u64,
) -> Result<Vec<usize>, StoreError> {
    super::select_turn_work_claim_indices(candidates, boundary, policy, now_epoch_ms)
        .map(|selection| selection.indices)
}

#[test]
fn claim_id_dialects_preserve_existing_spelling() {
    let cases = [
        (ClaimIdDialect::QueuedWork, "qwc:7:3"),
        (ClaimIdDialect::TurnInput, "tic:7:3"),
        (ClaimIdDialect::RecordingQueuedWork, "recording-qwc:7:3"),
        (ClaimIdDialect::RecordingTurnInput, "recording-tic:7:3"),
        (ClaimIdDialect::PerformanceQueuedWork, "perf-qwc:7:3"),
        (ClaimIdDialect::PerformanceTurnInput, "perf-tic:7:3"),
    ];
    for (dialect, expected) in cases {
        assert_eq!(derive_claim_id(dialect, 7, 3), expected);
    }
}

fn candidate(enqueue_seq: u64, merge_key: Option<&str>) -> ClaimCandidate {
    ClaimCandidate {
        batch_id: format!("qwb-{enqueue_seq}"),
        enqueue_seq,
        claim_fencing_token: 0,
        prior_claim_id: None,
        prior_claim_token: None,
        work_class: QueuedWorkClass::TurnWork,
        config_patch_command: false,
        delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
        kind: QueuedWorkKind::Turn,
        authority: QueuedWorkAuthority::new("principal"),
        merge_key: merge_key.map(str::to_string),
        enqueued_at_ms: 900,
        turn_causes: Vec::new(),
        input_texts: vec!["wake".to_string()],
    }
}

fn rendered_candidate_strategy() -> impl Strategy<Value = ClaimCandidate> {
    let merge_key = prop_oneof![
        Just(None),
        Just(Some("wake".to_string())),
        Just(Some("other".to_string())),
    ];
    let kind = prop_oneof![Just(QueuedWorkKind::Turn), Just(QueuedWorkKind::Control),];
    let work_class = prop_oneof![
        Just(QueuedWorkClass::TurnWork),
        Just(QueuedWorkClass::SessionCommand),
    ];
    let delivery_policy = prop_oneof![
        Just(DeliveryPolicy::EarliestSafeBoundary),
        Just(DeliveryPolicy::AfterCurrentTurnCommit),
    ];
    let authority = prop_oneof![
        Just(QueuedWorkAuthority::default()),
        Just(QueuedWorkAuthority::new("principal-a")),
        Just(QueuedWorkAuthority::new("principal-b").with_elevation("root")),
    ];
    let input_texts = vec(0usize..=256, 0..=3).prop_map(|lengths| {
        lengths
            .into_iter()
            .enumerate()
            .map(|(index, length)| ((b'a' + index as u8) as char).to_string().repeat(length))
            .collect::<Vec<_>>()
    });
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
                        process_id: format!("process-{index}"),
                        event_type: "wake".to_string(),
                        sequence: index as u64,
                        wake_id: Some(format!("wake-{index}")),
                        caused_by: None,
                    },
                    _ => crate::MessageOrigin::TurnInput {
                        turn_id: format!("turn-{index}"),
                        input_id: (index % 2 == 0).then(|| format!("input-{index}")),
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
        work_class,
        delivery_policy,
        authority,
        input_texts,
        turn_causes,
    )
        .prop_map(
            |(
                enqueue_seq,
                merge_key,
                kind,
                work_class,
                delivery_policy,
                authority,
                input_texts,
                turn_causes,
            )| ClaimCandidate {
                batch_id: format!("qwb-{enqueue_seq}"),
                enqueue_seq,
                claim_fencing_token: 0,
                prior_claim_id: None,
                prior_claim_token: None,
                work_class,
                config_patch_command: false,
                delivery_policy,
                kind,
                authority,
                merge_key,
                enqueued_at_ms: 900,
                turn_causes,
                input_texts,
            },
        )
}

#[test]
fn exact_selection_requires_the_literal_interrupted_composition() {
    let candidates = vec![
        ("w1".to_string(), Some("claim-a".to_string())),
        ("fresh".to_string(), None),
        ("w2".to_string(), Some("claim-a".to_string())),
    ];
    assert_eq!(
        select_interrupted_exact_claim_indices(&candidates, &["w1".to_string()]),
        Err(vec!["w1".to_string(), "w2".to_string()])
    );
    assert_eq!(
        select_interrupted_exact_claim_indices(&candidates, &["w1".to_string(), "w2".to_string()],),
        Ok(Some(vec![0, 2]))
    );

    let two_claims = vec![
        ("a1".to_string(), Some("claim-a".to_string())),
        ("a2".to_string(), Some("claim-a".to_string())),
        ("b1".to_string(), Some("claim-b".to_string())),
        ("b2".to_string(), Some("claim-b".to_string())),
    ];
    assert_eq!(
        select_interrupted_exact_claim_indices(
            &two_claims,
            &["a1".to_string(), "a2".to_string(), "b1".to_string()],
        ),
        Err(vec!["b1".to_string(), "b2".to_string()])
    );
    assert_eq!(
        select_interrupted_exact_claim_indices(
            &two_claims,
            &[
                "a1".to_string(),
                "a2".to_string(),
                "b1".to_string(),
                "b2".to_string(),
            ],
        ),
        Ok(Some(vec![0, 1]))
    );
}

fn policy(max_context_tokens: usize, action_token_reserve: usize) -> QueuedWorkClaimPolicy {
    QueuedWorkClaimPolicy {
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
    command_head.work_class = QueuedWorkClass::SessionCommand;
    let mut boundary_blocked = candidate(1, None);
    boundary_blocked.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;

    let cases: Vec<(&str, Vec<ClaimCandidate>, QueuedWorkClaimBoundary, _, _)> = vec![
        (
            "a host policy that admits no rows",
            vec![candidate(1, None)],
            QueuedWorkClaimBoundary::Idle,
            zero_row_policy,
            QueuedWorkClaimRefusal::ZeroLimit,
        ),
        (
            "an exhausted queue",
            Vec::new(),
            QueuedWorkClaimBoundary::Idle,
            policy(1_000, 100),
            QueuedWorkClaimRefusal::Empty,
        ),
        (
            "a session command at the queue head",
            vec![command_head],
            QueuedWorkClaimBoundary::Idle,
            policy(1_000, 100),
            QueuedWorkClaimRefusal::CommandAtHead,
        ),
        (
            "a head that may not cross the active turn boundary",
            vec![boundary_blocked],
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            policy(1_000, 100),
            QueuedWorkClaimRefusal::DeliveryBoundaryBlocked,
        ),
    ];
    for (scenario, candidates, boundary, claim_policy, expected) in cases {
        let selection =
            super::select_turn_work_claim_indices(&candidates, boundary, &claim_policy, 1_000)
                .expect("claim laws hold");
        assert!(
            selection.indices.is_empty(),
            "{scenario} must select nothing"
        );
        assert_eq!(selection.refusal, Some(expected), "{scenario}");
        let prefix =
            super::select_turn_work_claim_prefix(&candidates, boundary, &claim_policy, 1_000)
                .expect("claim laws hold");
        assert_eq!(prefix.len, 0, "{scenario}");
        assert_eq!(prefix.refusal, Some(expected), "{scenario}");
    }

    // A withheld head leaves a legal selection that no prefix-claiming
    // backend can take: the rows it may claim do not start at the head.
    let mut withheld_head = candidate(1, None);
    withheld_head.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;
    withheld_head.prior_claim_id = Some("qwc:1:1".to_string());
    let candidates = vec![withheld_head, candidate(2, None)];
    let selection = super::select_turn_work_claim_indices(
        &candidates,
        QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
        &policy(1_000, 100),
        1_000,
    )
    .expect("claim laws hold");
    assert_eq!(selection.indices, vec![1]);
    assert_eq!(selection.refusal, None);
    let prefix = super::select_turn_work_claim_prefix(
        &candidates,
        QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
        &policy(1_000, 100),
        1_000,
    )
    .expect("claim laws hold");
    assert_eq!(prefix.len, 0);
    assert_eq!(prefix.refusal, Some(QueuedWorkClaimRefusal::HeadWithheld));

    // `NotYetAvailable` has no scenario here on purpose: only a backend can see
    // that a lane still holds a row whose availability has not arrived, so the
    // cross-backend conformance suite pins it
    // (`queued_work_names_a_deferred_lane_apart_from_an_exhausted_one`).
}

/// The spellings travel into host logs and metrics labels, and they are the
/// same strings the claim-decision diagnostics have always emitted.
#[test]
fn refusal_spellings_are_stable() {
    let cases = [
        (QueuedWorkClaimRefusal::ZeroLimit, "zero_limit"),
        (QueuedWorkClaimRefusal::Empty, "empty"),
        (QueuedWorkClaimRefusal::NotYetAvailable, "not_yet_available"),
        (QueuedWorkClaimRefusal::CommandAtHead, "command_at_head"),
        (
            QueuedWorkClaimRefusal::DeliveryBoundaryBlocked,
            "delivery_boundary_blocked",
        ),
        (QueuedWorkClaimRefusal::HeadWithheld, "head_withheld"),
        (QueuedWorkClaimRefusal::ClaimRaceLost, "claim_race_lost"),
    ];
    for (refusal, expected) in cases {
        assert_eq!(refusal.as_str(), expected);
    }
}

#[test]
fn absent_merge_key_never_merges() {
    let candidates = vec![candidate(1, None), candidate(2, None)];
    assert_eq!(
        select_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
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
    let mut claim_policy = policy(1_000, 100);
    claim_policy.max_rows = 1;
    assert_eq!(
        select_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
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
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 100),
                1_000
            )
            .unwrap(),
            1
        );
    }
}

#[test]
fn control_kind_never_batches() {
    let mut first = candidate(1, Some("wake"));
    first.kind = QueuedWorkKind::Control;
    let candidates = vec![first, candidate(2, Some("wake"))];
    assert_eq!(
        select_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &policy(1_000, 100),
            1_000
        )
        .unwrap(),
        1
    );
}

#[test]
fn merge_key_delivery_and_work_class_mismatches_break_prefix() {
    let first = candidate(1, Some("a"));
    let mut different_delivery = candidate(2, Some("a"));
    different_delivery.delivery_policy = DeliveryPolicy::AfterCurrentTurnCommit;
    let mut command = candidate(2, Some("a"));
    command.work_class = QueuedWorkClass::SessionCommand;
    for candidates in [
        vec![first.clone(), candidate(2, Some("b"))],
        vec![first.clone(), different_delivery],
        vec![first.clone(), command],
    ] {
        assert_eq!(
            select_turn_work_claim_prefix(
                &candidates,
                QueuedWorkClaimBoundary::Idle,
                &policy(1_000, 100),
                1_000
            )
            .unwrap(),
            1
        );
    }
}

#[test]
fn the_default_drain_policy_claims_one_row_however_much_window_is_free() {
    let mut first = candidate(1, Some("wake"));
    first.input_texts = vec!["a".repeat(4)];
    let mut second = candidate(2, Some("wake"));
    second.input_texts = vec!["b".repeat(4)];
    // Both rows fit the window several times over; the shipped
    // one-at-a-time policy still drains only the head (FIG-1313).
    assert_eq!(
        select_turn_work_claim_prefix(
            &[first, second],
            QueuedWorkClaimBoundary::Idle,
            &policy(1_000, 300),
            1_000
        )
        .unwrap(),
        1
    );
}

#[test]
fn all_mode_claims_the_whole_compatible_prefix_without_token_arithmetic() {
    let candidates = vec![
        candidate(1, Some("wake")),
        candidate(2, Some("wake")),
        candidate(3, Some("wake")),
    ];
    let mut claim_policy = policy(101, 30);
    claim_policy.drain_policy =
        std::sync::Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
    // The three rows render past this deliberately tiny window. `All` is a
    // host statement that the provider is the authority on what fits, so
    // Lash coalesces every compatible row anyway.
    assert_eq!(
        select_turn_work_claim_indices(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
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
    let mut claim_policy = policy(1_000, 100);
    claim_policy.drain_policy = std::sync::Arc::new(GreedyPolicy);
    assert_eq!(
        select_turn_work_claim_indices(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
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
        select_turn_work_claim_indices(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &empty_policy,
            1_000,
        )
        .unwrap(),
        vec![0]
    );
}

#[test]
fn an_exact_host_selection_is_never_sized_by_the_automatic_drain_policy() {
    let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
    let claim_policy = policy(1_000, 100);
    assert_eq!(claim_policy.drain_policy.name(), "one_at_a_time");
    // Automatic drains take the head alone under the shipped default...
    assert_eq!(
        select_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
            1_000,
        )
        .unwrap(),
        1
    );
    // ...but the host named this exact two-row composition, and a partial
    // exact claim is abandoned as unclaimable by the caller, so shrinking it
    // would wedge `stream_selected_queued_work` forever.
    assert_eq!(
        select_exact_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
            1_000,
        )
        .unwrap(),
        2
    );
    // `max_rows` is exempt for the same reason: it bounds how many pending
    // rows Lash gathers on its own, and truncating a host-named composition
    // with it wedges the claim on a second axis. Redrive already exempts a
    // committed composition from a successor's row limit.
    let mut bounded = claim_policy.clone();
    bounded.max_rows = 1;
    assert_eq!(
        select_exact_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &bounded,
            1_000,
        )
        .unwrap(),
        2
    );
    // A genuine claim law still bounds it: incompatible rows never merge.
    let mut other_key = candidate(2, Some("other"));
    other_key.batch_id = "qwb-other".to_string();
    assert_eq!(
        select_exact_turn_work_claim_prefix(
            &[candidates[0].clone(), other_key],
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
            1_000,
        )
        .unwrap(),
        1
    );
}

#[test]
fn an_oversized_non_head_row_clamps_the_drain_instead_of_failing_it() {
    let mut first = candidate(1, Some("wake"));
    first.input_texts = vec!["a".repeat(8)];
    let mut second = candidate(2, Some("wake"));
    second.input_texts = vec!["b".repeat(4_000)];
    let third = candidate(3, Some("wake"));
    let mut claim_policy = policy(1_000, 100);
    claim_policy.drain_policy =
        std::sync::Arc::new(crate::DrainModePolicy::new(crate::DrainMode::All));
    // The fitting head still drains: the selection stops before the
    // oversized row rather than failing a claim that can make progress.
    assert_eq!(
        select_turn_work_claim_indices(
            &[first.clone(), second.clone(), third],
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
            1_000,
        )
        .unwrap(),
        vec![0]
    );
    // On the next wake the oversized row is the head, and it is refused
    // there by name rather than wedging the queue silently.
    let error = select_turn_work_claim_indices(
        &[second, first],
        QueuedWorkClaimBoundary::Idle,
        &claim_policy,
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
fn an_interrupted_redrive_never_consults_the_drain_policy() {
    #[derive(Debug)]
    struct PanickingPolicy;
    impl crate::QueuedDrainPolicy for PanickingPolicy {
        fn name(&self) -> &str {
            "test_panicking"
        }

        fn select_drain(
            &self,
            _request: &crate::QueuedDrainRequest<'_>,
        ) -> crate::QueuedDrainSelection {
            panic!("replayed drains must serve the journaled selection");
        }
    }

    let mut first = candidate(1, Some("wake"));
    first.prior_claim_id = Some("qwc:1:1".to_string());
    let mut second = candidate(2, Some("wake"));
    second.prior_claim_id = Some("qwc:1:1".to_string());
    let mut claim_policy = policy(1_000, 100);
    claim_policy.drain_policy = std::sync::Arc::new(PanickingPolicy);
    assert_eq!(
        select_turn_work_claim_indices(
            &[first, second],
            QueuedWorkClaimBoundary::Idle,
            &claim_policy,
            1_000,
        )
        .unwrap(),
        vec![0, 1]
    );
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
    first.input_texts = vec!["a".repeat(800)];
    assert_eq!(
        select_turn_work_claim_prefix(
            &[first],
            QueuedWorkClaimBoundary::Idle,
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
    first.input_texts = vec!["a".repeat(1_001)];
    assert!(matches!(
        select_turn_work_claim_prefix(
            &[first],
            QueuedWorkClaimBoundary::Idle,
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
        select_turn_work_claim_prefix(
            &[first],
            QueuedWorkClaimBoundary::ActiveTurnCheckpoint,
            &policy(1_000, 100),
            1_000,
        )
        .unwrap(),
        0
    );
}

#[test]
fn leading_session_command_blocks_turn_work_claim() {
    let mut command = candidate(1, None);
    command.work_class = QueuedWorkClass::SessionCommand;
    command.kind = QueuedWorkKind::Control;
    let candidates = vec![command, candidate(2, None)];
    assert_eq!(select_leading_session_command(&candidates), 1);
    assert_eq!(
        select_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &policy(1_000, 100),
            1_000
        )
        .unwrap(),
        0
    );
}

#[test]
fn adjacent_config_commands_share_one_claim_but_not_other_commands() {
    let mut first = candidate(1, None);
    first.work_class = QueuedWorkClass::SessionCommand;
    first.kind = QueuedWorkKind::Control;
    first.config_patch_command = true;
    let mut second = first.clone();
    second.batch_id = "qwb-2".to_string();
    second.enqueue_seq = 2;
    let mut refresh = second.clone();
    refresh.batch_id = "qwb-3".to_string();
    refresh.enqueue_seq = 3;
    refresh.config_patch_command = false;

    assert_eq!(select_leading_session_command(&[first, second, refresh]), 2);
}

#[test]
fn overdue_head_is_claimed_alone_at_claim_time() {
    let candidates = vec![candidate(1, Some("wake")), candidate(2, Some("wake"))];
    assert_eq!(
        select_turn_work_claim_prefix(
            &candidates,
            QueuedWorkClaimBoundary::Idle,
            &policy(1_000, 100),
            2_000
        )
        .unwrap(),
        1
    );
}

#[test]
fn lease_derivation_is_deterministic_and_advances_fencing() {
    let head = ClaimCandidate {
        batch_id: "qwb-7".to_string(),
        enqueue_seq: 7,
        claim_fencing_token: 2,
        prior_claim_id: None,
        prior_claim_token: None,
        work_class: QueuedWorkClass::TurnWork,
        config_patch_command: false,
        delivery_policy: DeliveryPolicy::EarliestSafeBoundary,
        kind: QueuedWorkKind::Turn,
        authority: QueuedWorkAuthority::default(),
        merge_key: None,
        enqueued_at_ms: 0,
        turn_causes: Vec::new(),
        input_texts: Vec::new(),
    };
    let owner = LeaseOwnerIdentity::opaque("owner", "owner:incarnation");
    let lease = WorkClaimLease::derive_queued_work(&head, "session", &owner, 1_000, 5)
        .expect("derive lease");
    assert_eq!(lease.fencing_token, 3);
    assert_eq!(lease.claim_id, "qwc:7:3");
    assert_eq!(lease.session_lease_generation, 5);
    let again = WorkClaimLease::derive_queued_work(&head, "session", &owner, 1_000, 5)
        .expect("derive lease again");
    assert_eq!(lease.lease_token, again.lease_token);
    assert_eq!(
        lease.lease_token,
        "d367bfde39e7d937ecbe2916cb02a5144a7221c318bcdb45aa2939567f176acc"
    );
}

#[test]
fn batch_id_includes_optional_nonce() {
    let plain = derive_batch_id("session", Some("key"), 1_000, None);
    let nonced = derive_batch_id("session", Some("key"), 1_000, Some(1));
    assert_ne!(plain, nonced);
    assert!(plain.starts_with("qwb:"));
}

#[test]
fn pending_session_ordering_compares_timestamps_only() {
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
    assert!(!precedes(Some(key(11, 1)), Some(key(10, 9))));
    // A timestamp tie resolves to the turn input whichever way the two
    // families' independent sequences happen to fall.
    assert!(!precedes(Some(key(10, 1)), Some(key(10, 2))));
    assert!(!precedes(Some(key(10, 2)), Some(key(10, 1))));
    assert!(!precedes(Some(key(10, 1)), Some(key(10, 1))));
    assert!(precedes(Some(key(10, 1)), None));
    assert!(!precedes(None, Some(key(10, 1))));
}
