//! Retained command receipts answer their first result after the head advances.
use super::*;
use pretty_assertions::assert_eq;

#[derive(Clone, Copy)]
enum Law {
    Receipt,
    ConfigOnce,
    Stale,
    Conflict,
}

fn transaction(model: &str) -> crate::ConfigTransaction {
    crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
        model: crate::LlmProfileKey::new(model),
    })
}

/// The keys these laws apply. A stale transaction's key is never minted, so
/// it is deliberately not served.
#[expect(clippy::expect_used, reason = "conformance fixture setup")]
fn command_llm_profiles() -> Arc<crate::LlmProfileRegistry> {
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .build()
        .into_handle();
    Arc::new(
        [SECOND_PROFILE, "newest-model"]
            .into_iter()
            .try_fold(crate::LlmProfileRegistry::new(), |registry, key| {
                registry.register(
                    key,
                    crate::RegisteredLlmProfile::new(
                        crate::testing::test_llm_profile_metadata(key),
                        provider.clone(),
                    ),
                )
            })
            .expect("register the command laws' models"),
    )
}

#[expect(clippy::expect_used, reason = "conformance fixture submission")]
async fn submit_config(
    runtime: &mut crate::LashRuntime,
    id: &str,
    revision: u64,
    model: &str,
) -> crate::SessionCommandReceipt {
    runtime
        .submit_config_transaction(id, revision, &transaction(model))
        .await
        .expect("submit config transaction")
}

#[expect(clippy::expect_used, reason = "conformance fixture engine execution")]
async fn work_and_settle(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ConfigParts,
    receipt: crate::SessionCommandReceipt,
    request: &'static str,
) -> crate::SessionCommandSettlement {
    let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt_parts = parts.clone();
    runner
        .run_turn(
            admit(crate::ExecutionScope::session_operation(
                &parts.session_id,
                request,
            )),
            Arc::new(move |controller| {
                let parts = attempt_parts.clone();
                let settled_tx = settled_tx.clone();
                let receipt = receipt.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    runtime
                        .execute_next_run(
                            request,
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                controller,
                            ),
                        )
                        .await
                        .expect("engine executes the command");
                    settled_tx
                        .send(
                            runtime
                                .settle_session_command(receipt)
                                .await
                                .expect("settlement"),
                        )
                        .expect("report actual engine execution");
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    settled_rx
        .recv()
        .await
        .expect("the runner executed the law")
}

fn config_outcome(settlement: crate::SessionCommandSettlement) -> crate::ConfigTransactionOutcome {
    match settlement {
        crate::SessionCommandSettlement::Applied {
            outcome: crate::runtime::SessionCommandOutcome::ConfigTransaction { outcome },
            ..
        } => outcome,
        other => panic!("a config transaction answers its typed outcome: {other:?}"),
    }
}

#[expect(clippy::expect_used, reason = "conformance fixture results")]
async fn command_law(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    law: Law,
) {
    let parts = law_session(
        prefix,
        "command-settlement",
        &effect_host,
        &stores,
        command_llm_profiles(),
    )
    .await;
    let mut runtime = build_runtime(parts.clone()).await;
    let first = if matches!(law, Law::Receipt | Law::Conflict) {
        runtime
            .submit_session_command(
                crate::SessionCommand::RefreshToolCatalog {
                    reason: "first".into(),
                },
                "first-key",
            )
            .await
            .expect("first receipt")
    } else {
        submit_config(&mut runtime, "first-key", 0, SECOND_PROFILE).await
    };
    let first_settlement = work_and_settle(&runner, &parts, first.clone(), "apply-first").await;
    let first_outcome = if matches!(law, Law::ConfigOnce | Law::Stale) {
        let outcome = config_outcome(first_settlement);
        assert!(matches!(
            outcome,
            crate::ConfigTransactionOutcome::Applied {
                base_revision: 0,
                revision: 1,
                ..
            }
        ));
        Some(outcome)
    } else {
        assert!(matches!(
            first_settlement,
            crate::SessionCommandSettlement::Durable(_)
        ));
        None
    };
    let first_commit = parts
        .store
        .queued_work_batch_completion(&parts.session_id, first.batch_id.as_str())
        .await
        .expect("first commit lookup")
        .expect("first applying receipt");

    if matches!(law, Law::Stale) {
        let stale = submit_config(&mut runtime, "stale-key", 0, "must-not-apply").await;
        let stale_outcome =
            config_outcome(work_and_settle(&runner, &parts, stale.clone(), "apply-stale").await);
        assert_eq!(
            stale_outcome,
            crate::ConfigTransactionOutcome::Stale {
                expected: 0,
                actual: 1
            }
        );
        let later = submit_config(&mut runtime, "later-key", 1, "newest-model").await;
        assert!(matches!(
            config_outcome(work_and_settle(&runner, &parts, later, "apply-later").await),
            crate::ConfigTransactionOutcome::Applied { revision: 2, .. }
        ));
        let replay = submit_config(&mut runtime, "stale-key", 0, "must-not-apply").await;
        assert_eq!(
            replay, stale,
            "a retained stale command answers its first receipt"
        );
        assert_eq!(
            config_outcome(
                runtime
                    .settle_session_command(replay)
                    .await
                    .expect("stale replay settlement")
            ),
            stale_outcome,
            "replay answers the recorded stale revision, not today's revision"
        );
        assert_eq!(runtime.export_persistence_state().config_revision, 2);
        assert_eq!(
            runtime
                .export_persistence_state()
                .policy
                .model
                .as_ref()
                .map(|model| model.key().as_str()),
            Some("newest-model")
        );
    } else {
        for (index, request) in ["apply-later-1", "apply-later-2", "apply-later-3"]
            .into_iter()
            .enumerate()
        {
            let revision = index as u64 + u64::from(matches!(law, Law::ConfigOnce));
            let later = submit_config(&mut runtime, request, revision, "newest-model").await;
            assert!(
                matches!(config_outcome(work_and_settle(&runner, &parts, later, request).await), crate::ConfigTransactionOutcome::Applied { base_revision, revision: next, .. } if base_revision == revision && next == revision + 1)
            );
        }
        if matches!(law, Law::Conflict) {
            let error = parts
                .store
                .enqueue_queued_work(
                    crate::QueuedWorkBatchDraft::new(
                        parts.session_id.clone(),
                        crate::DeliveryPolicy::AfterCurrentTurnCommit,
                        crate::SessionCommand::RefreshToolCatalog {
                            reason: "changed".into(),
                        },
                    )
                    .with_source_key(first.source_key),
                )
                .await
                .expect_err("changed content under a retained key is refused");
            assert!(
                matches!(error, crate::StoreError::QueuedWorkSourceKeyConflict { .. }),
                "the content conflict is typed: {error:?}"
            );
        } else {
            let replay = if matches!(law, Law::ConfigOnce) {
                submit_config(&mut runtime, "first-key", 0, SECOND_PROFILE).await
            } else {
                runtime
                    .submit_session_command(
                        crate::SessionCommand::RefreshToolCatalog {
                            reason: "first".into(),
                        },
                        "first-key",
                    )
                    .await
                    .expect("replay receipt")
            };
            assert_eq!(replay, first, "retained replay answers the first receipt");
            let replay_commit = parts
                .store
                .queued_work_batch_completion(&parts.session_id, replay.batch_id.as_str())
                .await
                .expect("replay commit lookup")
                .expect("retained applying receipt");
            assert_eq!(
                (replay_commit.head_revision, replay_commit.checkpoint_ref),
                (first_commit.head_revision, first_commit.checkpoint_ref),
                "replay answers the original applying receipt after later commits"
            );
            let settled = runtime
                .settle_session_command(replay)
                .await
                .expect("retained replay settlement");
            if let Some(first_outcome) = first_outcome {
                assert_eq!(config_outcome(settled), first_outcome);
                assert_eq!(runtime.export_persistence_state().config_revision, 4);
            } else {
                assert!(matches!(
                    settled,
                    crate::SessionCommandSettlement::Durable(_)
                ));
            }
        }
    }
    assert!(
        parts
            .store
            .list_queued_work(&parts.session_id)
            .await
            .expect("open work")
            .is_empty(),
        "retained replay enqueues no work"
    );
}

macro_rules! law {
    ($name:ident, $mode:ident) => {
        pub async fn $name(
            prefix: &str,
            effect_host: Arc<dyn crate::EffectHost>,
            stores: Arc<dyn crate::StoreSet>,
            runner: Arc<dyn crate::ConformanceTurnRunner>,
        ) {
            command_law(prefix, effect_host, stores, runner, Law::$mode).await;
        }
    };
}
law!(
    session_command_resubmission_after_advance_returns_first_receipt,
    Receipt
);
law!(
    settled_config_transaction_applies_once_after_advance,
    ConfigOnce
);
law!(
    stale_config_transaction_replay_returns_its_recorded_revision,
    Stale
);
law!(
    settled_command_changed_content_is_a_typed_conflict,
    Conflict
);
