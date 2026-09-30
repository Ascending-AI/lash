//! Store laws for engine-delivered accounting, independent of conversation commits.
#![expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "conformance laws establish each fixture and expected result"
)]
use super::deployment_view::DeploymentViewExt;
use lash_core::store::{AdmissionId, RetentionBound, RootStartNonce};
use lash_core::usage_accounting::*;
use lash_core::{
    DeploymentStore, LlmCallId, RuntimeOwner, SessionId, TokenUsage, UsageAccountingStore,
};
use std::future::Future;
use std::num::NonZeroU32;
use std::pin::Pin;
use std::sync::Arc;

/// A backend's complete non-accounting catalog rows, serialized deterministically.
pub type UsageLedgerSnapshot =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = Vec<(String, String)>> + Send>> + Send + Sync>;
pub struct UsageLedgerStoreFixture {
    pub accounting: Arc<dyn UsageAccountingStore>,
    pub factory: Arc<dyn DeploymentStore>,
    pub snapshot: UsageLedgerSnapshot,
}
fn owner(label: &str) -> RuntimeOwner {
    RuntimeOwner::Session(SessionId::from(label))
}
fn effect(label: &str) -> UsageEffectKey {
    UsageEffectKey::for_effect(
        &lash_sansio::EffectAddress::new(
            lash_sansio::ExecutionScope::runtime_operation("usage-store-law"),
            label,
        )
        .expect("effect address"),
    )
}
fn admission(
    owner: &RuntimeOwner,
    effect: &UsageEffectKey,
    run: UsageRunId,
    admitted_at_ms: u64,
) -> UsageRunAdmission {
    UsageRunAdmission {
        owner: owner.clone(),
        effect: effect.clone(),
        execution_scope_key: "scope".into(),
        run,
        source: "turn".into(),
        model: "model".into(),
        admitted_at_ms,
    }
}
fn facts() -> Vec<UsageAttemptFact> {
    (0..2)
        .flat_map(|call| {
            (0..2).map(move |attempt| UsageAttemptFact {
                call_ordinal: call,
                provider_attempt: attempt,
                llm_call_id: LlmCallId("deliberately-colliding-direct-id".into()),
                source: "turn".into(),
                model: "model".into(),
                outcome: if call == 1 && attempt == 0 {
                    AttemptFactOutcome::Unreported {
                        generation_id: Some("generation".into()),
                    }
                } else {
                    AttemptFactOutcome::Reported {
                        usage: TokenUsage {
                            input_tokens: 7,
                            output_tokens: 3,
                            cache_read_input_tokens: 2,
                            cache_write_input_tokens: 1,
                            reasoning_output_tokens: 4,
                        },
                        generation_id: Some("generation".into()),
                    }
                },
            })
        })
        .collect()
}
fn settlement(owner: &RuntimeOwner, effect: &UsageEffectKey, run: UsageRunId) -> UsageSettlement {
    UsageSettlement {
        owner: owner.clone(),
        effect: effect.clone(),
        run,
        facts: facts(),
        accounting: RunAccounting::Complete,
    }
}
async fn setup(f: &UsageLedgerStoreFixture) -> UsageSettlement {
    let s = settlement(&owner("usage-owner"), &effect("effect"), UsageRunId::mint());
    f.accounting
        .admit_usage_run(&admission(&s.owner, &s.effect, s.run.clone(), 10))
        .await
        .expect("admit");
    s
}
fn limit(n: u32) -> NonZeroU32 {
    NonZeroU32::new(n).expect("nonzero")
}
async fn all_facts(f: &UsageLedgerStoreFixture, owner: &RuntimeOwner) -> Vec<UsageFactRecord> {
    f.accounting
        .load_usage_fact_page(owner, None, limit(1000))
        .await
        .expect("facts")
        .facts
}

pub async fn identical_settlement_retry_is_a_no_op(f: &UsageLedgerStoreFixture) {
    let s = setup(f).await;
    assert_eq!(
        f.accounting
            .load_owner_usage(&s.owner)
            .await
            .unwrap()
            .completeness
            .oldest_open_admitted_at_ms,
        Some(10)
    );
    let first = f.accounting.settle_usage(&s, 20).await.unwrap();
    assert_eq!(first.inserted_facts, 4);
    let usage = f.accounting.load_owner_usage(&s.owner).await.unwrap();
    let retry = f.accounting.settle_usage(&s, 30).await.unwrap();
    assert_eq!(retry.inserted_facts, 0);
    assert_eq!(retry.duplicate_facts, 4);
    assert_eq!(retry.run, UsageRunResolution::Settled);
    assert_eq!(all_facts(f, &s.owner).await.len(), 4);
    assert_eq!(
        usage,
        f.accounting.load_owner_usage(&s.owner).await.unwrap()
    );
    assert_eq!(usage.rows[0].reported_attempts, 3);
    assert_eq!(usage.rows[0].usage.input_tokens, 21);
}

pub async fn conflicting_payload_is_a_typed_conflict_and_appends_nothing(
    f: &UsageLedgerStoreFixture,
) {
    let s = setup(f).await;
    f.accounting.settle_usage(&s, 20).await.unwrap();
    let other = UsageRunId::mint();
    f.accounting
        .admit_usage_run(&admission(&s.owner, &s.effect, other.clone(), 21))
        .await
        .unwrap();
    let before = all_facts(f, &s.owner).await;
    let mut changed = s.clone();
    let mut new = changed.facts[0].clone();
    new.call_ordinal = 99;
    changed.facts.insert(0, new);
    if let AttemptFactOutcome::Reported { usage, .. } = &mut changed.facts[1].outcome {
        usage.input_tokens += 1;
    }
    let Err(UsageAppendError::Conflict(conflict)) = f.accounting.settle_usage(&changed, 30).await
    else {
        panic!("expected a typed conflict");
    };
    assert_eq!(conflict.identity.effect, s.effect);
    assert_eq!(conflict.identity.call_ordinal, 0);
    assert_eq!(conflict.identity.provider_attempt, 0);
    assert_eq!(conflict.identity.kind, UsageFactKind::Attempt);
    assert_eq!(
        conflict.stored_payload_hash,
        usage_fact_payload_hash(&s.facts[0], &s.run)
    );
    assert_eq!(
        conflict.offered_payload_hash,
        usage_fact_payload_hash(&changed.facts[1], &changed.run)
    );
    assert_ne!(conflict.stored_payload_hash, conflict.offered_payload_hash);
    assert_eq!(all_facts(f, &s.owner).await, before);
    let runs = f
        .accounting
        .load_usage_run_page(&s.owner, UsageRunFilter::All, None, limit(10))
        .await
        .unwrap()
        .runs;
    assert!(
        runs.iter()
            .any(|r| r.run == s.run && r.state == UsageRunState::Settled)
    );
    f.accounting
        .mark_usage_settlement_conflicted(&changed, &conflict, 31)
        .await
        .unwrap();
    let unresolved = f
        .accounting
        .load_usage_run_page(&s.owner, UsageRunFilter::Unresolved, None, limit(10))
        .await
        .unwrap()
        .runs;
    assert!(
        unresolved
            .iter()
            .any(|r| r.run == other && matches!(r.state, UsageRunState::Conflicted { .. }))
    );
    assert_eq!(all_facts(f, &s.owner).await, before);
}

pub async fn a_correction_has_its_own_identity(f: &UsageLedgerStoreFixture) {
    let s = setup(f).await;
    f.accounting.settle_usage(&s, 20).await.unwrap();
    let correction = UsageCorrection {
        effect: s.effect.clone(),
        call_ordinal: 1,
        provider_attempt: 0,
        usage: TokenUsage {
            input_tokens: 11,
            output_tokens: 5,
            ..Default::default()
        },
        generation_id: "generation".into(),
    };
    assert_eq!(
        f.accounting
            .append_usage_corrections(&s.owner, std::slice::from_ref(&correction), 30)
            .await
            .unwrap()
            .inserted,
        1
    );
    let usage = f.accounting.load_owner_usage(&s.owner).await.unwrap();
    assert_eq!(usage.rows[0].usage.input_tokens, 32);
    assert_eq!(usage.rows[0].reconciled_attempts, 1);
    assert!(usage.outstanding.is_empty());
    assert!(usage.completeness.is_complete());
    assert_eq!(usage.report().usage.total_tokens, 55);
    assert_eq!(
        f.accounting
            .append_usage_corrections(&s.owner, std::slice::from_ref(&correction), 40)
            .await
            .unwrap()
            .duplicates,
        1
    );
    let mut changed = correction.clone();
    changed.usage.input_tokens += 1;
    assert!(matches!(
        f.accounting
            .append_usage_corrections(&s.owner, &[changed], 50)
            .await,
        Err(UsageAppendError::Conflict(_))
    ));
    let mut missing = correction.clone();
    missing.call_ordinal = 99;
    assert!(matches!(
        f.accounting
            .append_usage_corrections(&s.owner, &[missing], 50)
            .await,
        Err(UsageAppendError::CorrectionTargetMissing { .. })
    ));
    let mut reported = correction;
    reported.call_ordinal = 0;
    assert!(matches!(
        f.accounting
            .append_usage_corrections(&s.owner, &[reported], 50)
            .await,
        Err(UsageAppendError::CorrectionTargetReported { .. })
    ));
    assert_eq!(all_facts(f, &s.owner).await.len(), 5);
}

pub async fn each_fact_counts_once_under_any_grouping_order_and_repeat(
    f: &UsageLedgerStoreFixture,
) {
    use proptest::prelude::*;
    let accounting = f.accounting.clone();
    let runtime = tokio::runtime::Handle::current();
    tokio::task::spawn_blocking(move || {
        let strategy = proptest::collection::vec((0_u8..4, 0_u8..4, 1_u8..8), 1..48);
        let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
            cases: 24,
            failure_persistence: None,
            ..Default::default()
        });
        runner
            .run(&strategy, |schedule| {
                runtime.block_on(async {
                    let owner = owner(&format!("grouping-{}", UsageRunId::mint().as_str()));
                    let runs: Vec<_> = (0..4).map(|_| UsageRunId::mint()).collect();
                    let mut distinct = std::collections::BTreeSet::new();
                    for (key, start, width) in schedule {
                        let mut s = UsageSettlement {
                            owner: owner.clone(),
                            effect: effect(&format!("effect-{key}")),
                            run: runs[key as usize].clone(),
                            facts: Vec::new(),
                            accounting: RunAccounting::Complete,
                        };
                        for offset in 0..width {
                            let ordinal = (u32::from(start) + u32::from(offset)) % 4;
                            distinct.insert((key, ordinal));
                            s.facts.push(UsageAttemptFact {
                                call_ordinal: ordinal,
                                provider_attempt: 0,
                                llm_call_id: LlmCallId("same-call".into()),
                                source: "turn".into(),
                                model: "model".into(),
                                outcome: AttemptFactOutcome::Reported {
                                    usage: TokenUsage {
                                        input_tokens: 7,
                                        ..Default::default()
                                    },
                                    generation_id: None,
                                },
                            });
                        }
                        accounting.settle_usage(&s, 20).await.unwrap();
                        accounting.settle_usage(&s, 30).await.unwrap();
                    }
                    let usage = accounting.load_owner_usage(&owner).await.unwrap();
                    prop_assert_eq!(usage.rows[0].reported_attempts, distinct.len() as u64);
                    prop_assert_eq!(usage.rows[0].usage.input_tokens, distinct.len() as i64 * 7);
                    Ok(())
                })
            })
            .expect("usage grouping property");
    })
    .await
    .unwrap();
}

pub async fn admission_is_idempotent_and_retirement_fences_it(f: &UsageLedgerStoreFixture) {
    let s = setup(f).await;
    let a = admission(&s.owner, &s.effect, s.run.clone(), 10);
    assert_eq!(
        f.accounting.admit_usage_run(&a).await.unwrap(),
        UsageRunAdmitted::AlreadyAdmitted
    );
    assert_eq!(
        f.accounting
            .load_owner_usage(&s.owner)
            .await
            .unwrap()
            .completeness
            .open_runs,
        1
    );
    let retired = f.accounting.retire_usage_owner(&s.owner, 20).await.unwrap();
    assert_eq!(retired.resolved_open_runs, 1);
    assert!(!retired.already_retired);
    let retry = f.accounting.retire_usage_owner(&s.owner, 30).await.unwrap();
    assert_eq!(retry.retired_at_ms, 20);
    assert!(retry.already_retired);
    assert_eq!(retry.resolved_open_runs, 0);
    let Err(UsageAdmissionError::OwnerRetired {
        owner,
        retired_at_ms,
    }) = f
        .accounting
        .admit_usage_run(&admission(&s.owner, &s.effect, UsageRunId::mint(), 40))
        .await
    else {
        panic!("owner must be retired");
    };
    assert_eq!(owner, s.owner);
    assert_eq!(retired_at_ms, 20);
    assert_eq!(
        f.accounting
            .load_usage_run_page(&s.owner, UsageRunFilter::Unresolved, None, limit(10))
            .await
            .unwrap()
            .runs[0]
            .state,
        UsageRunState::Unknown(UsageUnknownReason::OwnerRetired)
    );
    f.accounting.settle_usage(&s, 50).await.unwrap();
    assert_eq!(
        f.accounting
            .load_owner_usage(&s.owner)
            .await
            .unwrap()
            .completeness
            .unknown_runs,
        0
    );
    // Race retirement with admission. Any admitted run must already be resolved when both return.
    for index in 0..8 {
        let owner = self::owner(&format!("race-{index}"));
        let a = admission(&owner, &effect("race"), UsageRunId::mint(), 1);
        let (admitted, retired) = tokio::join!(
            f.accounting.admit_usage_run(&a),
            f.accounting.retire_usage_owner(&owner, 2)
        );
        retired.unwrap();
        assert!(
            admitted.is_ok() || matches!(admitted, Err(UsageAdmissionError::OwnerRetired { .. }))
        );
        assert_eq!(
            f.accounting
                .load_owner_usage(&owner)
                .await
                .unwrap()
                .completeness
                .open_runs,
            0
        );
    }
}

pub async fn settlement_resolves_superseded_runs_unknown(f: &UsageLedgerStoreFixture) {
    let s = setup(f).await;
    let r1 = UsageRunId::mint();
    f.accounting
        .admit_usage_run(&admission(&s.owner, &s.effect, r1.clone(), 11))
        .await
        .unwrap();
    let receipt = f.accounting.settle_usage(&s, 20).await.unwrap();
    assert_eq!(receipt.superseded_runs, 1);
    let usage = f.accounting.load_owner_usage(&s.owner).await.unwrap();
    assert_eq!(usage.completeness.unknown_runs, 1);
    assert_eq!(usage.completeness.open_runs, 0);
    let runs = f
        .accounting
        .load_usage_run_page(&s.owner, UsageRunFilter::Unresolved, None, limit(10))
        .await
        .unwrap();
    assert_eq!(runs.runs[0].run, r1);
    assert_eq!(
        runs.runs[0].state,
        UsageRunState::Unknown(UsageUnknownReason::SupersededRun)
    );
    // Execution retirement only resolves open runs in the selected scope.
    let mut other = admission(&s.owner, &effect("other"), UsageRunId::mint(), 12);
    other.execution_scope_key = "other-scope".into();
    f.accounting.admit_usage_run(&other).await.unwrap();
    let a = admission(&s.owner, &effect("unrecorded"), UsageRunId::mint(), 13);
    f.accounting.admit_usage_run(&a).await.unwrap();
    assert_eq!(
        f.accounting
            .retire_usage_execution(&s.owner, "scope", 30)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        f.accounting
            .retire_usage_execution(&s.owner, "scope", 31)
            .await
            .unwrap(),
        0
    );
    assert_eq!(
        f.accounting
            .load_owner_usage(&s.owner)
            .await
            .unwrap()
            .completeness
            .open_runs,
        1
    );
}

pub async fn a_late_settlement_supersedes_retirement(f: &UsageLedgerStoreFixture) {
    let s = setup(f).await;
    f.accounting.retire_usage_owner(&s.owner, 20).await.unwrap();
    f.accounting.settle_usage(&s, 30).await.unwrap();
    let usage = f.accounting.load_owner_usage(&s.owner).await.unwrap();
    assert!(usage.completeness.retired);
    assert_eq!(usage.completeness.unknown_runs, 0);
    assert_eq!(usage.completeness.open_runs, 0);
    assert_eq!(all_facts(f, &s.owner).await.len(), 4);
    for (suffix, accounting, reason) in [
        (
            "cancel",
            RunAccounting::CallWithoutRecord { calls: 1 },
            UsageUnknownReason::CallWithoutRecord,
        ),
        (
            "poison",
            RunAccounting::FactsUnjournalable { dropped_facts: 4 },
            UsageUnknownReason::FactsUnjournalable,
        ),
    ] {
        let s = UsageSettlement {
            owner: s.owner.clone(),
            effect: effect(suffix),
            run: UsageRunId::mint(),
            facts: Vec::new(),
            accounting,
        };
        assert_eq!(
            f.accounting.settle_usage(&s, 40).await.unwrap().run,
            UsageRunResolution::Unknown(reason)
        );
    }
}

pub async fn accounting_writes_never_touch_head_fence_or_receipts(f: &UsageLedgerStoreFixture) {
    let id = SessionId::from("accounting-independent");
    let request = super::session_store_factory::session_store_request(
        &id,
        "model",
        lash_core::SessionRelation::Root,
    );
    let view = f.factory.admit_view(&request).await.unwrap();
    let mut state = lash_core::RuntimeSessionState {
        session_id: id.clone(),
        ..lash_core::RuntimeSessionState::new(request.config.session_policy())
    };
    state.ensure_agent_frame_initialized();
    view.commit_runtime_state(lash_core::RuntimeCommit::persisted_state_for_test(
        &state,
        &[],
    ))
    .await
    .unwrap();
    let store = view.store();
    let head = store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            &id,
            lash_core::TurnInputIngress::NextTurn,
            lash_core::TurnInput::text("accounting independence"),
        ))
        .await
        .unwrap()
        .input_id;
    let seal = store
        .seal_drive_epoch(
            &id,
            &AdmissionId::new("sealed"),
            0,
            &RootStartNonce::new("root-start"),
        )
        .await
        .unwrap();
    let lash_core::store::DriveEpochSeal::Sealed(fence) = seal else {
        panic!("the initial drive must seal");
    };
    let mut root = lash_core::testing::store_fixtures::admit_root_request_for_test(
        &fence,
        &lash_core::TurnId::from("accounting-independent-root"),
        lash_core::store::AdmittedHead::Input(head),
    );
    let head = store.load_session_head_meta(&id).await.unwrap().unwrap();
    root.base = lash_core::store::SessionHeadRef {
        generation: 0,
        revision: head.head_revision,
        leaf: head.leaf_node_id,
        checkpoint: head.checkpoint_ref,
    };
    assert!(store.admit_root(&root).await.unwrap().is_some());
    f.factory.begin_session_close(&id, 5).await.unwrap();
    let before = (f.snapshot)().await;
    let s = settlement(
        &RuntimeOwner::Session(id.clone()),
        &effect("independent"),
        UsageRunId::mint(),
    );
    f.accounting
        .admit_usage_run(&admission(&s.owner, &s.effect, s.run.clone(), 10))
        .await
        .unwrap();
    f.accounting.settle_usage(&s, 20).await.unwrap();
    let correction = UsageCorrection {
        effect: s.effect.clone(),
        call_ordinal: 1,
        provider_attempt: 0,
        usage: TokenUsage::default(),
        generation_id: "generation".into(),
    };
    f.accounting
        .append_usage_corrections(&s.owner, &[correction], 30)
        .await
        .unwrap();
    f.accounting.retire_usage_owner(&s.owner, 40).await.unwrap();
    assert_eq!((f.snapshot)().await, before);
    f.factory.delete_session(&id).await.unwrap();
    let deleted = (f.snapshot)().await;
    let s = settlement(&s.owner, &effect("after-delete"), UsageRunId::mint());
    f.accounting.settle_usage(&s, 50).await.unwrap();
    assert_eq!((f.snapshot)().await, deleted);
    assert_eq!(all_facts(f, &s.owner).await.len(), 9);
}

pub async fn retention_reclaims_only_retired_owners_before_the_horizon(
    f: &UsageLedgerStoreFixture,
) {
    for (label, retired) in [
        ("live", None),
        ("old", Some(100)),
        ("recent", Some(300)),
        ("at-bound", Some(200)),
    ] {
        let s = settlement(&owner(label), &effect("retained"), UsageRunId::mint());
        f.accounting
            .admit_usage_run(&admission(&s.owner, &s.effect, s.run.clone(), 10))
            .await
            .unwrap();
        f.accounting.settle_usage(&s, 20).await.unwrap();
        if let Some(at) = retired {
            f.accounting.retire_usage_owner(&s.owner, at).await.unwrap();
        }
    }
    let report = f
        .factory
        .reclaim_retained_evidence(RetentionBound {
            committed_before_epoch_ms: 200,
        })
        .await
        .unwrap();
    assert_eq!(report.removed_usage_fact_count, 4);
    assert_eq!(report.removed_usage_run_count, 1);
    assert_eq!(report.removed_usage_owner_retirement_count, 1);
    assert_eq!(all_facts(f, &owner("old")).await.len(), 0);
    assert!(
        !f.accounting
            .load_owner_usage(&owner("old"))
            .await
            .unwrap()
            .completeness
            .retired
    );
    for label in ["live", "recent", "at-bound"] {
        assert_eq!(all_facts(f, &owner(label)).await.len(), 4);
        assert_eq!(
            f.accounting
                .load_usage_run_page(&owner(label), UsageRunFilter::All, None, limit(10))
                .await
                .unwrap()
                .runs
                .len(),
            1
        );
    }
    assert_eq!(
        f.factory
            .reclaim_retained_evidence(RetentionBound {
                committed_before_epoch_ms: 200
            })
            .await
            .unwrap()
            .removed_usage_fact_count,
        0
    );
}

pub async fn reads_select_by_owner_without_a_committed_turn(f: &UsageLedgerStoreFixture) {
    let owners = [
        owner("no-session"),
        RuntimeOwner::Process(lash_sansio::ProcessId::fixture("usage-process")),
    ];
    for owner in &owners {
        let s = settlement(owner, &effect("receipt-free"), UsageRunId::mint());
        f.accounting.settle_usage(&s, 20).await.unwrap();
        let usage = f.accounting.load_owner_usage(owner).await.unwrap();
        assert_eq!(usage.rows[0].usage.input_tokens, 21);
        let first = f
            .accounting
            .load_usage_fact_page(owner, None, limit(2))
            .await
            .unwrap();
        assert_eq!(first.facts.len(), 2);
        let cursor = first.next.unwrap();
        assert!(
            cursor
                .check_owner(&owners[usize::from(owner == &owners[0])])
                .is_err()
        );
        let second = f
            .accounting
            .load_usage_fact_page(owner, Some(&cursor), limit(2))
            .await
            .unwrap();
        assert_eq!(second.facts.len(), 2);
        assert!(second.next.is_none());
        assert!(second.facts[0].seq > first.facts[1].seq);
        assert!(
            f.accounting
                .load_usage_fact_page(
                    &owners[usize::from(owner == &owners[0])],
                    Some(&cursor),
                    limit(2)
                )
                .await
                .is_err()
        );
        let extra = UsageSettlement {
            effect: effect("second-effect"),
            facts: Vec::new(),
            ..s
        };
        f.accounting.settle_usage(&extra, 21).await.unwrap();
        let first = f
            .accounting
            .load_usage_run_page(owner, UsageRunFilter::All, None, limit(1))
            .await
            .unwrap();
        let cursor = first.next.unwrap();
        let second = f
            .accounting
            .load_usage_run_page(owner, UsageRunFilter::All, Some(&cursor), limit(1))
            .await
            .unwrap();
        assert_eq!(second.runs.len(), 1);
        assert!(second.next.is_none());
        assert!(
            f.accounting
                .load_usage_run_page(
                    &owners[usize::from(owner == &owners[0])],
                    UsageRunFilter::All,
                    Some(&cursor),
                    limit(1)
                )
                .await
                .is_err()
        );
    }
}
