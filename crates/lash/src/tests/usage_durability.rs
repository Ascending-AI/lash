//! FIG-2765: billed-but-unreported calls must survive a restart.
//!
//! The owner's usage accounting is the only place that knows a call was
//! billed and never counted (ADR 0125). These witnesses drive the whole loop
//! through a real store round trip — unreported attempt, reconciliation,
//! park, reopen — and pin the cancellation schedule that used to eat pending
//! work when a lookup future was dropped.

use super::*;

const SEED: u64 = 0x5c_f10a;

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
                stream.send(LlmStreamEvent::Delta {
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: "<typescript>\nfinish(\"sealed\");\n</typescript>\n".to_string(),
                });
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
async fn usage_durability_core(
    provider: ProviderHandle,
) -> Result<(LashCore, lash_restate_test::RestateTestBackend)> {
    let (core, _store_factory, double) = usage_durability_core_with_store(provider).await?;
    Ok((core, double))
}

async fn usage_durability_core_with_store(
    provider: ProviderHandle,
) -> Result<(
    LashCore,
    Arc<dyn lash_core::DeploymentStore>,
    lash_restate_test::RestateTestBackend,
)> {
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let store_factory = backend.session_store_factory();
    let core = explicit_ephemeral_facets(rlm_core_builder_over(backend.clone()))
    .serve_test_model(provider, mock_model_spec())
    // The default 2000 ms drain would make every witness below wait on a
    // deadline that is not what is under test.
    .abort_drain_grace(Duration::from_millis(50))
    .build(crate::testing::runtime_lease_owner())?;
    Ok((core, store_factory, double))
}

/// An unreported attempt is an accounting fact of the session's owner, so it
/// survives close and reopen with its attribution, and reading it never talks
/// to the provider.
#[test]
fn unreported_attempts_survive_close_and_reopen_with_their_attribution() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-unreported-survives-reopen", || async {
        let log = Arc::new(LookupLog::default());
        let (core, _double) = usage_durability_core(aborting_provider(
            "fig2765-unreported",
            vec![Some("gen-alpha"), None],
            |_| None,
            Arc::clone(&log),
        ))
        .await?;

        let session = core
            .session("fig2765-unreported")
            .created()
            .await
            .open()
            .await?;
        let first = session.send(TurnInput::text("one")).output().await?;
        let second = session.send(TurnInput::text("two")).output().await?;
        let first_call = first.result.llm_calls[0].call_id.clone();
        let second_call = second.result.llm_calls[0].call_id.clone();
        assert_ne!(first_call, second_call);
        let live = settled_usage(&session).await?.outstanding;
        assert_eq!(live.len(), 2, "one outstanding attempt per aborted call");
        Box::pin(session.close()).await?;

        let reopened = core
            .session("fig2765-unreported")
            .created()
            .await
            .open()
            .await?;
        let usage = settled_usage(&reopened).await?;
        assert_eq!(
            usage.outstanding, live,
            "reopening reads every outstanding attempt with its original attribution"
        );
        let by_call = |call_id: &lash_core::LlmCallId| {
            usage
                .outstanding
                .iter()
                .find(|attempt| &attempt.llm_call_id == call_id)
                .cloned()
                .expect("outstanding attempt for call")
        };
        let alpha = by_call(&first_call);
        assert_eq!(alpha.provider_attempt, 1);
        assert_eq!(alpha.source, "turn");
        assert_eq!(alpha.requested_model, mock_model_spec().wire_model);
        assert_eq!(alpha.generation_id.as_deref(), Some("gen-alpha"));
        // An attempt with no generation id is a fact, not missing data: it
        // stays outstanding as unreconcilable rather than disappearing.
        assert_eq!(by_call(&second_call).generation_id, None);

        let report = usage.report();
        assert_eq!(report.usage.unreported_attempts, 2);
        assert_eq!(report.usage.reconciled_attempts, 0);
        assert_eq!(report.usage.total_tokens, 0);
        assert!(
            log.snapshot().is_empty(),
            "reading usage must not talk to the provider"
        );
        Box::pin(reopened.close()).await?;
        Ok(())
    })
}

/// A correction is appended to the owner's accounting when it is recovered,
/// so closing the session right after reconciling loses nothing, and a
/// repeated reconciliation asks the provider nothing.
#[test]
fn a_correction_survives_close_and_repeat_reconciliation_is_a_no_op() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-correction-survives-close", || async {
        let log = Arc::new(LookupLog::default());
        let (core, _double) = usage_durability_core(aborting_provider(
            "fig2765-correction",
            vec![Some("gen-alpha")],
            |generation_id| (generation_id == "gen-alpha").then(|| reconciled(334)),
            Arc::clone(&log),
        ))
        .await?;

        let session = core
            .session("fig2765-correction")
            .created()
            .await
            .open()
            .await?;
        session.send(TurnInput::text("one")).output().await?;
        settled_usage(&session).await?;
        Box::pin(session.close()).await?;

        let reopened = core
            .session("fig2765-correction")
            .created()
            .await
            .open()
            .await?;
        let report = reopened.reconcile_unreported_usage().await?;
        assert_eq!(report.reconciled.len(), 1);
        assert!(report.unresolved.is_empty());
        assert_eq!(report.reconciled[0].usage.input_tokens, 334);
        Box::pin(reopened.close()).await?;

        let after = core
            .session("fig2765-correction")
            .created()
            .await
            .open()
            .await?;
        let usage = settled_usage(&after).await?;
        let totals = usage.report().usage;
        assert_eq!(totals.usage.input_tokens, 334);
        assert_eq!(totals.total_tokens, 334);
        assert_eq!(totals.reconciled_attempts, 1);
        assert_eq!(totals.unreported_attempts, 0);
        assert!(usage.outstanding.is_empty());

        let repeat = after.reconcile_unreported_usage().await?;
        assert!(repeat.reconciled.is_empty());
        assert!(repeat.unresolved.is_empty());
        assert_eq!(
            log.snapshot(),
            vec!["gen-alpha".to_string()],
            "a filled attempt is never looked up again"
        );
        assert_eq!(
            settled_usage(&after).await?.report().usage,
            totals,
            "totals do not move"
        );
        Box::pin(after.close()).await?;

        // The same survival through park/resume rather than close/open.
        let resumed_session = core
            .session("fig2765-correction")
            .created()
            .await
            .open()
            .await?;
        let parked = Box::pin(resumed_session.park()).await?;
        let resumed = Box::pin(core.resume(parked)).await?;
        assert_eq!(settled_usage(&resumed).await?.report().usage, totals);
        Box::pin(resumed.close()).await?;
        Ok(())
    })
}

/// Reconciliation appends each correction as it is recovered, so dropping the
/// future keeps every correction it recorded and leaves every attempt it did
/// not finish outstanding.
#[test]
fn dropping_a_reconciliation_future_keeps_unfinished_attempts_outstanding() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-cancel-reconciliation", || async {
        let log = Arc::new(LookupLog::default());
        let block = Arc::new(AtomicBool::new(true));
        let entered = Arc::new(tokio::sync::Notify::new());
        let (core, _double) = {
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
                        stream.send(LlmStreamEvent::Delta {
                            block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                            text: "<typescript>\nfinish(\"sealed\");\n</typescript>\n".to_string(),
                        });
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
            usage_durability_core(provider).await?
        };

        let session = core
            .session("fig2765-cancel")
            .created()
            .await
            .open()
            .await?;
        session.send(TurnInput::text("one")).output().await?;
        session.send(TurnInput::text("two")).output().await?;
        assert_eq!(settled_usage(&session).await?.outstanding.len(), 2);

        let mut pending = Box::pin(session.reconcile_unreported_usage());
        tokio::select! {
            _ = &mut pending => panic!("the blocked second lookup must not complete"),
            _ = entered.notified() => {}
        }
        drop(pending);

        // The first attempt's correction is recorded and it is no longer
        // outstanding; the second was still in flight and stays outstanding.
        let still_open = settled_usage(&session).await?.outstanding;
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
        let totals = settled_usage(&session).await?.report().usage;
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
        Box::pin(session.close()).await?;

        let reopened = core
            .session("fig2765-cancel")
            .created()
            .await
            .open()
            .await?;
        let usage = settled_usage(&reopened).await?;
        let totals = usage.report().usage;
        assert_eq!(totals.usage.input_tokens, 333);
        assert_eq!(totals.reconciled_attempts, 2);
        assert_eq!(totals.unreported_attempts, 0);
        assert!(usage.outstanding.is_empty());
        Box::pin(reopened.close()).await?;
        Ok(())
    })
}

/// Usage accounting never rides a session commit (ADR 0125): outstanding
/// attempts, a reconciliation and the parks around them leave the session
/// head where the turn left it.
#[test]
fn reconciliation_and_park_never_move_the_session_head() -> Result<()> {
    run_async_test_on_stack_budget("fig2765-park-head-revision", || async {
        let log = Arc::new(LookupLog::default());
        let (core, store_factory, _double) = usage_durability_core_with_store(aborting_provider(
            "fig2765-park",
            vec![Some("gen-alpha")],
            |generation_id| (generation_id == "gen-alpha").then(|| reconciled(334)),
            Arc::clone(&log),
        ))
        .await?;
        let session_id = "fig2765-park";
        let head_revision = || {
            let store_factory = Arc::clone(&store_factory);
            async move {
                lash_core::SessionCommitStore::load_session_head_meta(
                    store_factory.as_ref(),
                    &SessionId::from(session_id),
                )
                .await
                .expect("read settled head")
                .expect("session exists")
                .head_revision
            }
        };

        let session = core.session(session_id).created().await.open().await?;
        session.send(TurnInput::text("one")).output().await?;
        settled_usage(&session).await?;
        Box::pin(session.close()).await?;
        let after_turn = head_revision().await;

        let quiet = core.session(session_id).created().await.open().await?;
        assert_eq!(settled_usage(&quiet).await?.outstanding.len(), 1);
        Box::pin(quiet.close()).await?;
        assert_eq!(
            head_revision().await,
            after_turn,
            "a clean park must stay a durable no-op"
        );

        let reconciling = core.session(session_id).created().await.open().await?;
        reconciling.reconcile_unreported_usage().await?;
        Box::pin(reconciling.close()).await?;
        assert_eq!(
            head_revision().await,
            after_turn,
            "a correction is accounting, not a session commit"
        );

        let quiet_again = core.session(session_id).created().await.open().await?;
        assert_eq!(
            settled_usage(&quiet_again)
                .await?
                .report()
                .usage
                .usage
                .input_tokens,
            334
        );
        Box::pin(quiet_again.close()).await?;
        assert_eq!(head_revision().await, after_turn);
        Ok(())
    })
}

/// E8 (FIG-4236, ADR 0125): reconciliation reads the outstanding attempts
/// from the ledger, not from a host's memory, and a correction has its own
/// identity. Two hosts over the same deployment each reconcile the aborted
/// attempt: the first appends its one correction, the second finds nothing
/// outstanding, and the ledger holds exactly one correction fact.
#[test]
fn reconciliation_appends_one_correction() -> Result<()> {
    run_async_test_on_stack_budget("fig4236-reconciliation-two-hosts", || async {
        let log = Arc::new(LookupLog::default());
        let provider = |log: &Arc<LookupLog>| {
            aborting_provider(
                "fig4236-reconcile",
                vec![Some("gen-alpha")],
                |generation_id| (generation_id == "gen-alpha").then(|| reconciled(55)),
                Arc::clone(log),
            )
        };
        let (first_host, double) = usage_durability_core(provider(&log)).await?;
        let session = first_host
            .session("fig4236-reconcile")
            .created()
            .await
            .open()
            .await?;
        session.send(TurnInput::text("one")).output().await?;
        let before = settled_usage(&session).await?;
        assert_eq!(
            before.outstanding.len(),
            1,
            "one aborted attempt is outstanding"
        );
        let report = session.reconcile_unreported_usage().await?;
        assert_eq!(report.reconciled.len(), 1);
        Box::pin(session.close()).await?;

        let second_host = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
            .serve_test_model(provider(&log), mock_model_spec())
            .abort_drain_grace(Duration::from_millis(50))
            .build(crate::testing::runtime_lease_owner())?;
        let reopened = second_host
            .session("fig4236-reconcile")
            .created()
            .await
            .open()
            .await?;
        let repeat = reopened.reconcile_unreported_usage().await?;
        assert!(
            repeat.reconciled.is_empty(),
            "the second host finds it corrected"
        );
        assert!(repeat.unresolved.is_empty());
        let after = settled_usage(&reopened).await?;
        assert!(after.outstanding.is_empty(), "nothing is outstanding");
        let owner = lash_core::RuntimeOwner::Session(SessionId::from("fig4236-reconcile"));
        let page = second_host
            .usage_fact_page(
                &owner,
                None,
                std::num::NonZeroU32::new(64).expect("nonzero"),
            )
            .await?;
        let corrections = page
            .facts
            .iter()
            .filter(|fact| fact.disposition == lash_core::UsageReporting::Reconciled)
            .count();
        assert_eq!(corrections, 1, "exactly one correction: {:#?}", page.facts);
        assert_eq!(
            log.snapshot(),
            vec!["gen-alpha".to_string()],
            "the provider is asked about the generation once"
        );
        Box::pin(reopened.close()).await?;
        Ok(())
    })
}
