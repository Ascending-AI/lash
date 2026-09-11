//! FIG-2765 fix round: billed-but-unreported calls must survive a restart.
//!
//! The ledger is the only place that knows a call was billed and never counted.
//! These witnesses drive the whole loop through a real store round trip — hole,
//! reconciliation, park, reopen — and pin the cancellation schedule that used to
//! eat pending work when a lookup future was dropped.

use super::*;

use lash_core::provider::ReconciledUsage;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Every generation id a reconciliation lookup asked about, in order, so a test
/// can prove an already-filled attempt is never looked up twice.
#[derive(Default)]
struct LookupLog {
    generations: StdMutex<Vec<String>>,
}

impl LookupLog {
    fn record(&self, generation_id: &str) {
        self.generations
            .lock_recover()
            .push(generation_id.to_string());
    }

    fn snapshot(&self) -> Vec<String> {
        self.generations.lock_recover().clone()
    }
}

fn reconciled(input_tokens: i64) -> ReconciledUsage {
    ReconciledUsage {
        usage: lash_core::llm::types::LlmUsage {
            input_tokens,
            ..lash_core::llm::types::LlmUsage::default()
        },
        provider_usage: serde_json::json!({ "cancelled": true }),
    }
}

/// A provider whose streamed attempt announces a generation id and then never
/// returns, so the abort drain seals it as billed-but-unreported.
#[cfg(feature = "rlm")]
fn aborting_provider(
    kind: &'static str,
    generation_ids: Vec<Option<&'static str>>,
    reconcile: impl Fn(String) -> Option<ReconciledUsage> + Send + Sync + 'static,
    log: Arc<LookupLog>,
) -> ProviderHandle {
    let generation_ids = Arc::new(StdMutex::new(VecDeque::from(generation_ids)));
    crate::testing::TestProvider::builder()
        .kind(kind)
        .requires_streaming(true)
        .complete(move |request| {
            let generation_id = generation_ids.lock_recover().pop_front().flatten();
            async move {
                let stream = request.stream_events.expect("stream events");
                // Evidence is only admissible once the response is established,
                // so the generation id rides the first delta, not before it.
                stream.send(LlmStreamEvent::Delta(
                    "<lashlang>\nfinish \"sealed\"\n</lashlang>\n".to_string(),
                ));
                if let Some(generation_id) = generation_id {
                    stream.send(LlmStreamEvent::Evidence(
                        lash_core::llm::types::LlmStreamEvidence {
                            // Execution evidence is only admissible once the
                            // provider marks the response as established.
                            response_started: true,
                            http_summary: Some("200 OK".to_string()),
                            execution_evidence: Some(lash_core::ExecutionEvidence {
                                provider_response_id: Some(generation_id.to_string()),
                                ..lash_core::ExecutionEvidence::default()
                            }),
                            ..lash_core::llm::types::LlmStreamEvidence::default()
                        },
                    ));
                }
                std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>().await
            }
        })
        .reconcile_usage(move |generation_id| {
            let recovered = reconcile(generation_id.clone());
            log.record(&generation_id);
            async move { Ok(recovered) }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
fn usage_durability_core(provider: ProviderHandle) -> Result<LashCore> {
    Ok(usage_durability_core_with_store(provider)?.0)
}

#[cfg(feature = "rlm")]
fn usage_durability_core_with_store(
    provider: ProviderHandle,
) -> Result<(
    LashCore,
    Arc<lash_core::facade_support::InMemorySessionStoreFactory>,
)> {
    let store_factory = Arc::new(lash_core::facade_support::InMemorySessionStoreFactory::new());
    let core = explicit_ephemeral_facets(LashCore::rlm_builder(
        lash_core::TurnBudget::Unbounded,
        rlm_factory(),
    ))
    .provider(provider)
    .model(mock_model_spec())
    // The default 2000 ms drain would make every witness below wait on a
    // deadline that is not what is under test.
    .abort_drain_grace(Duration::from_millis(50))
    .store_factory(Arc::clone(&store_factory) as Arc<dyn SessionStoreFactory>)
    .process_registry(Arc::new(TestLocalProcessRegistry::default()))
    .build(crate::testing::runtime_lease_owner())?;
    Ok((core, store_factory))
}

/// The defect this closes: before the fix both store read paths rebuilt every
/// ledger row with a defaulted disposition and nothing repopulated the pending
/// registry, so reopening a session turned a billed call into a free one.
#[cfg(feature = "rlm")]
#[test]
fn unreported_holes_survive_close_and_reopen_with_their_attribution() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-hole-survives-reopen", || async {
        let log = Arc::new(LookupLog::default());
        let core = usage_durability_core(aborting_provider(
            "fig2765-hole",
            vec![Some("gen-alpha"), None],
            |_| None,
            Arc::clone(&log),
        ))?;

        let session = core.session("fig2765-hole").open().await?;
        let first = session.turn(TurnInput::text("one")).run().await?;
        let second = session.turn(TurnInput::text("two")).run().await?;
        let first_call = first.result.llm_calls[0].call_id.0.clone();
        let second_call = second.result.llm_calls[0].call_id.0.clone();
        assert_ne!(first_call, second_call);
        let mut live = session.unreported_usage_attempts().await;
        assert_eq!(live.len(), 2, "one hole per aborted attempt");
        session.close().await?;

        let reopened = core.session("fig2765-hole").open().await?;
        let mut restored = reopened.unreported_usage_attempts().await;
        // Durable rows carry their holes in canonical key order, so compare the
        // sets rather than the order they happened to be registered in.
        live.sort_by(|a, b| (&a.call_id, a.attempt_ordinal).cmp(&(&b.call_id, b.attempt_ordinal)));
        restored
            .sort_by(|a, b| (&a.call_id, a.attempt_ordinal).cmp(&(&b.call_id, b.attempt_ordinal)));
        assert_eq!(
            restored, live,
            "reopening must rebuild every hole with its original attribution"
        );
        let by_call = |call_id: &str| {
            restored
                .iter()
                .find(|attempt| attempt.call_id == call_id)
                .cloned()
                .expect("hole for call")
        };
        let alpha = by_call(&first_call);
        assert_eq!(alpha.attempt_ordinal, 1);
        assert_eq!(alpha.source, "turn");
        assert_eq!(alpha.model, mock_model_spec().id);
        assert_eq!(alpha.generation_id.as_deref(), Some("gen-alpha"));
        // A hole with no generation id is a fact, not missing data: it survives
        // the reload as an unreconcilable hole rather than disappearing.
        assert_eq!(by_call(&second_call).generation_id, None);

        let report = reopened.usage_report();
        assert_eq!(report.usage.unreported_attempts, 2);
        assert_eq!(report.usage.reconciled_attempts, 0);
        assert_eq!(report.usage.total_tokens, 0);
        assert!(
            log.snapshot().is_empty(),
            "reopening must not talk to the provider"
        );
        reopened.close().await?;
        Ok(())
    })
}

/// The defect this closes: reconciliation appended its correction to the shared
/// pending ledger, and park's flush predicate ignored that ledger, so closing a
/// session immediately after a successful reconciliation dropped the recovered
/// charge on the floor.
#[cfg(feature = "rlm")]
#[test]
fn a_correction_survives_close_and_repeat_reconciliation_is_a_no_op() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-correction-survives-close", || async {
        let log = Arc::new(LookupLog::default());
        let core = usage_durability_core(aborting_provider(
            "fig2765-correction",
            vec![Some("gen-alpha")],
            |generation_id| (generation_id == "gen-alpha").then(|| reconciled(334)),
            Arc::clone(&log),
        ))?;

        let session = core.session("fig2765-correction").open().await?;
        session.turn(TurnInput::text("one")).run().await?;
        session.close().await?;

        let reopened = core.session("fig2765-correction").open().await?;
        let report = reopened.reconcile_unreported_usage().await?;
        assert_eq!(report.reconciled.len(), 1);
        assert!(report.unresolved.is_empty());
        assert_eq!(report.reconciled[0].usage.input_tokens, 334);
        // Closing immediately: the correction has not ridden any other
        // boundary, so park is the only thing that can persist it.
        reopened.close().await?;

        let after = core.session("fig2765-correction").open().await?;
        let totals = after.usage_report().usage;
        assert_eq!(totals.usage.input_tokens, 334);
        assert_eq!(totals.total_tokens, 334);
        assert_eq!(totals.reconciled_attempts, 1);
        assert_eq!(totals.unreported_attempts, 0);
        assert!(after.unreported_usage_attempts().await.is_empty());

        let repeat = after.reconcile_unreported_usage().await?;
        assert!(repeat.reconciled.is_empty());
        assert!(repeat.unresolved.is_empty());
        assert_eq!(
            log.snapshot(),
            vec!["gen-alpha".to_string()],
            "a filled attempt is never looked up again"
        );
        assert_eq!(after.usage_report().usage, totals, "totals do not move");
        after.close().await?;

        // The same survival through park/resume rather than close/open.
        let resumed_session = core.session("fig2765-correction").open().await?;
        let parked = resumed_session.park().await?;
        let resumed = Box::pin(core.resume(parked)).await?;
        assert_eq!(resumed.usage_report().usage, totals);
        resumed.close().await?;
        Ok(())
    })
}

/// The defect this closes: `reconcile_unreported_usage` took the pending vector
/// with `mem::take` before its first await, so dropping the future erased every
/// attempt it had not reached while the durable holes stayed open.
#[cfg(feature = "rlm")]
#[test]
fn dropping_a_reconciliation_future_keeps_unfinished_attempts_registered() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-cancel-reconciliation", || async {
        let log = Arc::new(LookupLog::default());
        let block = Arc::new(AtomicBool::new(true));
        let entered = Arc::new(tokio::sync::Notify::new());
        let core = {
            let block = Arc::clone(&block);
            let entered = Arc::clone(&entered);
            let log = Arc::clone(&log);
            let generation_ids = Arc::new(StdMutex::new(VecDeque::from(vec![
                Some("gen-first"),
                Some("gen-second"),
            ])));
            let provider = crate::testing::TestProvider::builder()
                .kind("fig2765-cancel")
                .requires_streaming(true)
                .complete(move |request| {
                    let generation_id = generation_ids.lock_recover().pop_front().flatten();
                    async move {
                        let stream = request.stream_events.expect("stream events");
                        stream.send(LlmStreamEvent::Delta(
                            "<lashlang>\nfinish \"sealed\"\n</lashlang>\n".to_string(),
                        ));
                        if let Some(generation_id) = generation_id {
                            stream.send(LlmStreamEvent::Evidence(
                                lash_core::llm::types::LlmStreamEvidence {
                                    response_started: true,
                                    http_summary: Some("200 OK".to_string()),
                                    execution_evidence: Some(lash_core::ExecutionEvidence {
                                        provider_response_id: Some(generation_id.to_string()),
                                        ..lash_core::ExecutionEvidence::default()
                                    }),
                                    ..lash_core::llm::types::LlmStreamEvidence::default()
                                },
                            ));
                        }
                        std::future::pending::<std::result::Result<LlmResponse, LlmTransportError>>(
                        )
                        .await
                    }
                })
                .reconcile_usage(move |generation_id| {
                    log.record(&generation_id);
                    let blocking = generation_id == "gen-second" && block.load(Ordering::Acquire);
                    let entered = Arc::clone(&entered);
                    async move {
                        if blocking {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                        }
                        Ok(Some(reconciled(if generation_id == "gen-first" {
                            111
                        } else {
                            222
                        })))
                    }
                })
                .build()
                .into_handle();
            usage_durability_core(provider)?
        };

        let session = core.session("fig2765-cancel").open().await?;
        session.turn(TurnInput::text("one")).run().await?;
        session.turn(TurnInput::text("two")).run().await?;
        assert_eq!(session.unreported_usage_attempts().await.len(), 2);

        let mut pending = Box::pin(session.reconcile_unreported_usage());
        tokio::select! {
            _ = &mut pending => panic!("the blocked second lookup must not complete"),
            _ = entered.notified() => {}
        }
        drop(pending);

        // The first attempt's correction is recorded and it is off the
        // registry; the second was still in flight and stays registered.
        let still_open = session.unreported_usage_attempts().await;
        assert_eq!(
            still_open.len(),
            1,
            "a dropped lookup must not consume the work it never finished"
        );
        assert_eq!(
            still_open[0].generation_id.as_deref(),
            Some("gen-second"),
            "the attempt that was mid-lookup is the one that stays open"
        );

        // Retrying finishes the work exactly once more, without re-asking for
        // the attempt that already has a correction.
        block.store(false, Ordering::Release);
        let report = session.reconcile_unreported_usage().await?;
        assert_eq!(
            report.reconciled.len(),
            1,
            "only the attempt the dropped future never finished is left to do"
        );
        assert!(report.unresolved.is_empty());
        let totals = session.usage_report().usage;
        assert_eq!(
            totals.usage.input_tokens, 333,
            "the correction the dropped future did record was kept"
        );
        assert_eq!(totals.reconciled_attempts, 2);
        assert_eq!(totals.unreported_attempts, 0);
        assert_eq!(
            log.snapshot(),
            vec![
                "gen-first".to_string(),
                "gen-second".to_string(),
                "gen-second".to_string()
            ],
            "the completed first attempt is never looked up twice"
        );
        session.close().await?;

        let reopened = core.session("fig2765-cancel").open().await?;
        let totals = reopened.usage_report().usage;
        assert_eq!(totals.usage.input_tokens, 333);
        assert_eq!(totals.reconciled_attempts, 2);
        assert_eq!(totals.unreported_attempts, 0);
        assert!(reopened.unreported_usage_attempts().await.is_empty());
        reopened.close().await?;
        Ok(())
    })
}

/// Park stays a durable no-op when nothing is pending — including when the
/// session carries durable unresolved holes, which are already committed — and
/// commits exactly once when a correction is waiting.
#[cfg(feature = "rlm")]
#[test]
fn park_commits_for_a_pending_correction_and_stays_a_no_op_otherwise() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-park-head-revision", || async {
        let log = Arc::new(LookupLog::default());
        let (core, store_factory) = usage_durability_core_with_store(aborting_provider(
            "fig2765-park",
            vec![Some("gen-alpha")],
            |generation_id| (generation_id == "gen-alpha").then(|| reconciled(334)),
            Arc::clone(&log),
        ))?;
        let session_id = "fig2765-park";
        let head_revision = || {
            let store_factory = Arc::clone(&store_factory);
            async move {
                let store = SessionStoreFactory::open_existing_store_by_id(
                    store_factory.as_ref(),
                    &SessionId::from(session_id),
                )
                .await
                .expect("open settled store")
                .expect("session exists");
                <dyn lash_core::store::RuntimePersistence>::load_session(store.as_ref())
                    .await
                    .expect("read settled head")
                    .expect("session exists")
                    .head_revision
            }
        };

        let session = core.session(session_id).open().await?;
        session.turn(TurnInput::text("one")).run().await?;
        session.close().await?;
        let after_turn = head_revision().await;

        // Durable unresolved holes on their own are not pending work: opening
        // and closing again must not bump the head.
        let quiet = core.session(session_id).open().await?;
        assert_eq!(quiet.unreported_usage_attempts().await.len(), 1);
        quiet.close().await?;
        assert_eq!(
            head_revision().await,
            after_turn,
            "a clean park must stay a durable no-op"
        );

        // One pending correction, one commit.
        let reconciling = core.session(session_id).open().await?;
        reconciling.reconcile_unreported_usage().await?;
        reconciling.close().await?;
        let after_correction = head_revision().await;
        assert_eq!(
            after_correction,
            after_turn + 1,
            "a pending correction causes exactly one durable commit"
        );

        let quiet_again = core.session(session_id).open().await?;
        assert_eq!(quiet_again.usage_report().usage.usage.input_tokens, 334);
        quiet_again.close().await?;
        assert_eq!(
            head_revision().await,
            after_correction,
            "a subsequent clean park causes none"
        );
        Ok(())
    })
}
