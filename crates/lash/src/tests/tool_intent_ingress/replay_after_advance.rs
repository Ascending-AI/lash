//! A redelivered submission answers from its recorded admission after the
//! store moved on (FIG-4324, ADR 0105 §1).
//!
//! The tool-intent family of the replay-after-advance laws. A redelivery
//! after a crash arrives on a fresh invocation with an empty effect journal,
//! so the only record it can decide from is the submission ledger row its
//! first delivery wrote. Each law records one submission, moves the store on
//! (the target ended, pruned and compacted, or the start's session deleted),
//! and requires the redelivery to answer the recorded outcome and realize
//! nothing again. The Restate-hosted legs of the other families live in
//! `crates/lash/tests/replay_after_advance.rs`; this family runs here because
//! the ingress on Restate only runs inside a handler scope.

use super::*;

fn ingress_of(core: &LashCore) -> Result<crate::tools::ToolIntentIngress> {
    core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))
}

/// Which intent a law submits.
#[derive(Clone, Copy, Debug)]
enum Intent {
    Start,
    Signal,
    Cancel,
}

/// How a law moves the store on after the first delivery recorded.
#[derive(Clone, Copy, Debug)]
enum Advance {
    /// The target ends, retention prunes it and compacts its tombstone:
    /// nothing names it.
    PruneAndCompact,
    /// The session the start was granted is deleted.
    DeleteSession,
}

async fn intent_for(core: &LashCore, intent: Intent, target: &ProcessId) -> lash_core::ToolIntent {
    let owner = crate::RuntimeOwner::Session(SessionId::from(SESSION));
    match intent {
        Intent::Start => {
            let scope = core
                .processes()
                .session_scope(&SessionId::from(SESSION))
                .await
                .expect("the law's session is live");
            lash_core::ToolIntent::StartProcess(Box::new(lash_core::StartProcessIntent {
                owner,
                declaration: lash_core::ProcessStartDeclaration::external(
                    lash_core::ProcessOriginator::host(),
                    serde_json::json!({"law": "replay-after-advance"}),
                    lash_core::Lifetime::Until(scope),
                ),
            }))
        }
        Intent::Signal => lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
            owner,
            process_id: target.clone(),
            signal_name: SIGNAL.to_string(),
            payload: serde_json::json!({"law": "replay-after-advance"}),
        }),
        Intent::Cancel => lash_core::ToolIntent::CancelProcess(lash_core::CancelProcessIntent {
            owner,
            process_id: target.clone(),
        }),
    }
}

/// End `process_id` (a cancel may already have), prune it and compact its
/// tombstone.
async fn end_prune_and_compact(registry: &Arc<dyn ProcessRegistry>, process_id: &ProcessId) {
    let record = registry
        .get_process(process_id)
        .await
        .expect("read the target")
        .expect("the target is retained");
    // An externally owned row is closed by its owner; a row lash runs is
    // closed by its workflow key, the engine's single writer.
    let authority = if record.input.is_externally_owned() {
        lash_core::ProcessCompletionAuthority::external_owner()
    } else {
        lash_core::ProcessCompletionAuthority::workflow_key(process_id.to_string())
    };
    let ended = match registry
        .complete_process(
            process_id,
            lash_core::ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success(
                serde_json::json!({"done": true}),
            )),
            authority,
        )
        .await
    {
        Ok(ended) => ended.updated_at_ms,
        Err(lash_core::PluginError::ProcessAlreadyTerminal { .. }) => {
            registry
                .get_process(process_id)
                .await
                .expect("read the ended target")
                .expect("the ended target is retained")
                .updated_at_ms
        }
        Err(error) => panic!("end the target: {error:?}"),
    };
    let pruned = registry
        .prune_terminal_processes(
            ended.saturating_add(1),
            None,
            lash_core::ProjectionWatermark::NoProjector,
        )
        .await
        .expect("prune the ended target");
    assert!(pruned.pruned_processes >= 1, "the target is pruned");
    registry
        .compact_process_tombstones(
            u64::MAX / 2,
            lash_core::ProjectionWatermark::NoProjector,
            None,
        )
        .await
        .expect("compact the target's tombstone");
    assert!(
        registry
            .get_process(process_id)
            .await
            .expect("read a compacted id")
            .is_none(),
        "a compacted target is unknown"
    );
}

/// A second invocation over `first`'s durable backend with a fresh effect
/// journal, as [`second_invocation_of`] builds it, without reopening the
/// law's session: a law may have deleted it.
fn redelivery_of(first: &LashCore) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        ingress_backend(
            first.backend().clone(),
            Some(Arc::new(KeyJournalController::default())),
            None,
        ),
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .plugin(lash_core::testing::process_engine_plugin_fixture())
    .build(crate::testing::runtime_lease_owner())
}

async fn redelivery_answers_the_recorded_outcome(intent: Intent, advance: Advance) -> Result<()> {
    let (core, registry, target) = ingress_core(sqlite_memory_store_backend().await).await?;
    let submitted = intent_for(&core, intent, &target).await;
    let key = ingress_of(&core)?
        .key("replay-after-advance", 0)
        .expect("a host submission handle");
    let first = ingress_of(&core)?
        .submit(key.clone(), submitted.clone())
        .await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome: recorded,
        replayed: false,
    } = &first
    else {
        panic!("the first delivery is admitted fresh, got {first:?}");
    };
    match advance {
        Advance::PruneAndCompact => {
            let pruned = match intent {
                Intent::Start => started_process_id(&first),
                Intent::Signal | Intent::Cancel => target.clone(),
            };
            end_prune_and_compact(&registry, &pruned).await;
        }
        Advance::DeleteSession => {
            lash_core::SessionCatalogStore::delete_session(
                core.backend().session_store_factory().as_ref(),
                &SessionId::from(SESSION),
            )
            .await
            .expect("delete the start's session");
        }
    }
    let processes_before = registered_process_count(&registry).await?;

    let redelivery = redelivery_of(&core)?;
    let redelivered = ingress_of(&redelivery)?.submit(key, submitted).await;
    assert_eq!(
        redelivered,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: recorded.clone(),
            replayed: true,
        },
        "{intent:?} after {advance:?}: the redelivery answers the recorded outcome"
    );
    assert_eq!(
        registered_process_count(&registry).await?,
        processes_before,
        "{intent:?} after {advance:?}: the redelivery registers nothing"
    );
    Ok(())
}

/// A redelivered trigger emission answers its recorded report after the
/// child its delivery started is pruned and compacted, and starts nothing.
#[tokio::test]
async fn trigger_redelivery_after_delivery_prune_answers_the_recorded_outcome() -> Result<()> {
    let (core, _store, _subscription, registry) = ingress_core_with_trigger_store(
        sqlite_memory_store_backend().await,
        Arc::new(KeyJournalController::default()),
    )
    .await?;
    let key = ingress_of(&core)?
        .key("replay-after-advance-trigger", 0)
        .expect("a host submission handle");
    let first = ingress_of(&core)?
        .submit(key.clone(), trigger_intent(&SessionId::from(SESSION)))
        .await;
    let crate::tools::ToolIntentIngressOutcome::Admitted {
        outcome:
            recorded @ lash_core::ToolIntentExecutionOutcome::Executed {
                kind: lash_core::ToolIntentKind::EmitTrigger,
                result,
                ..
            },
        replayed: false,
    } = &first
    else {
        panic!("the first delivery emits fresh, got {first:?}");
    };
    let delivered: ProcessId =
        serde_json::from_value(result["deliveries"][0]["process_id"].clone())
            .expect("the emit started one delivery");
    end_prune_and_compact(&registry, &delivered).await;
    let processes_before = registered_process_count(&registry).await?;

    let redelivery = redelivery_of(&core)?;
    let redelivered = ingress_of(&redelivery)?
        .submit(key, trigger_intent(&SessionId::from(SESSION)))
        .await;
    assert_eq!(
        redelivered,
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: recorded.clone(),
            replayed: true,
        },
        "the redelivered emission answers the recorded report"
    );
    assert_eq!(
        registered_process_count(&registry).await?,
        processes_before,
        "the redelivered emission starts no delivery"
    );
    Ok(())
}

#[tokio::test]
async fn start_redelivery_after_session_delete_answers_the_recorded_outcome() -> Result<()> {
    Box::pin(redelivery_answers_the_recorded_outcome(
        Intent::Start,
        Advance::DeleteSession,
    ))
    .await
}

#[tokio::test]
async fn start_redelivery_after_prune_answers_the_recorded_outcome() -> Result<()> {
    Box::pin(redelivery_answers_the_recorded_outcome(
        Intent::Start,
        Advance::PruneAndCompact,
    ))
    .await
}

#[tokio::test]
async fn signal_redelivery_after_prune_answers_the_recorded_outcome() -> Result<()> {
    Box::pin(redelivery_answers_the_recorded_outcome(
        Intent::Signal,
        Advance::PruneAndCompact,
    ))
    .await
}

#[tokio::test]
async fn cancel_redelivery_after_prune_answers_the_recorded_outcome() -> Result<()> {
    Box::pin(redelivery_answers_the_recorded_outcome(
        Intent::Cancel,
        Advance::PruneAndCompact,
    ))
    .await
}
