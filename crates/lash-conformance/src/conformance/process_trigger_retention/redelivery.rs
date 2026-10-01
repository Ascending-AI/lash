//! A redelivered emission on a host that journals nothing never writes a
//! reclaimed occurrence or delivery back (FIG-4513).
//!
//! A host with no journal runs an emission's ingest again on every
//! redelivery. Once retention has reclaimed the occurrence, the ingest finds
//! no row under the idempotency key. The trigger store answers from the
//! tombstone the reclaim left: it refuses the ingest as reclaimed and writes
//! nothing, so the emission reserves and starts nothing.
//!
//! The tombstone outlives every redelivery (FIG-4573): no reclaim cutoff
//! compacts it inside `TRIGGER_OCCURRENCE_REDELIVERY_HORIZON_MS`.

use super::*;
use pretty_assertions::assert_eq;

const SOURCE_TYPE: &str = "ui.button.pressed";

/// Everything the trigger store and the registry hold that an emission could
/// have written.
#[derive(Debug, PartialEq)]
struct Held {
    occurrences: Vec<crate::TriggerOccurrenceRecord>,
    deliveries: Vec<crate::TriggerDeliveryReservation>,
    processes: usize,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn held(handles: &ProcessTriggerRetentionHandles) -> Held {
    Held {
        occurrences: handles
            .triggers
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list occurrences"),
        deliveries: handles
            .triggers
            .list_deliveries()
            .await
            .expect("list deliveries"),
        processes: handles
            .registry
            .list_processes(&crate::ProcessListFilter {
                status: crate::ProcessStatusFilter::Any,
                ..crate::ProcessListFilter::default()
            })
            .await
            .expect("list processes")
            .len(),
    }
}

/// A router over the law's stores whose `test` engine target is admitted.
fn router(handles: &ProcessTriggerRetentionHandles) -> lash_core::facade_support::TriggerRouter {
    lash_core::facade_support::TriggerRouter::new(
        Arc::clone(&handles.triggers),
        crate::ProcessWorkWiring::without_process_work(Arc::clone(&handles.registry)),
    )
    .with_process_artifacts(
        Arc::clone(&handles.process_env),
        crate::ProcessEngineRegistry::new().with_registration(
            crate::ProcessEngineRegistration::accepting(Arc::new(TriggerTargetEngine)),
        ),
    )
}

/// A host that journals nothing: it runs every step's body in place, a
/// process start included, on the first delivery and on every redelivery.
struct JournalLessHost;

impl crate::AwaitEventResolver for JournalLessHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        None
    }
}

#[async_trait::async_trait]
impl crate::RuntimeEffectController for JournalLessHost {
    async fn execute_effect(
        &self,
        envelope: crate::RuntimeEffectEnvelope,
        local_executor: crate::RuntimeEffectLocalExecutor<'_>,
    ) -> Result<crate::RuntimeEffectOutcome, crate::RuntimeEffectControllerError> {
        crate::testing::execute_effect_locally(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        _group: crate::RuntimeEffectGroup,
    ) -> Result<crate::EffectGroupHandle, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("JournalLessHost"))
    }

    async fn await_next_settlement(
        &self,
        _handle: &mut crate::EffectGroupHandle,
        _cancel: crate::TurnCancelWait,
    ) -> Result<crate::GroupSettlement, crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("JournalLessHost"))
    }

    async fn close_effect_group(
        &self,
        _handle: crate::EffectGroupHandle,
        _disposition: crate::LoserPolicy,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Err(crate::effect_groups_unsupported("JournalLessHost"))
    }
}

/// One delivery of `request` on a host that journals nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn deliver(
    handles: &ProcessTriggerRetentionHandles,
    request: &crate::TriggerOccurrenceRequest,
) -> Result<lash_core::facade_support::TriggerEmitReport, crate::PluginError> {
    let controller = JournalLessHost;
    let scoped = crate::ScopedEffectController::borrowed(
        &controller,
        crate::admit(crate::ExecutionScope::runtime_operation(format!(
            "redelivery:{}",
            request.idempotency_key
        ))),
    )
    .expect("scope the emission");
    router(handles)
        .emit_recorded(request.clone(), &scoped)
        .await
}

/// The redelivery of `request` is refused as reclaimed, and neither store
/// holds anything it did not hold before.
async fn assert_redelivery_writes_nothing(
    handles: &ProcessTriggerRetentionHandles,
    request: &crate::TriggerOccurrenceRequest,
    case: &str,
) {
    let before = held(handles).await;
    match deliver(handles, request).await {
        Err(error) => assert!(
            crate::is_trigger_occurrence_reclaimed(&error),
            "{case}: the redelivery is refused as reclaimed, got {error:?}"
        ),
        Ok(report) => panic!("{case}: the redelivery ran the emission again: {report:?}"),
    }
    assert_eq!(
        held(handles).await,
        before,
        "{case}: the redelivery wrote a row or started a process"
    );
}

/// A reclaim pass at the widest cutoff a host can name, `u64::MAX`, run inside
/// the redelivery horizon: it compacts no tombstone.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_the_widest_cutoff_compacts_nothing(triggers: &Arc<dyn TriggerStore>, case: &str) {
    let pass = triggers
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("run the reclaim pass");
    assert_eq!(
        (
            pass.reclaimed_occurrence_count,
            pass.compacted_tombstone_count
        ),
        (0, 0),
        "{case}: a pass inside the redelivery horizon compacted a tombstone"
    );
}

/// A matched emission whose process was pruned, and whose occurrence and
/// delivery retention then reclaimed.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_redelivered_emission_writes_no_reclaimed_delivery_back(
    handles: ProcessTriggerRetentionHandles,
) {
    let session = SessionId::from("redelivery-matched-session");
    let spec = crate::ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        crate::SessionPolicy::new(crate::TurnBudget::Unbounded),
    );
    let env_ref = spec.stable_ref().expect("environment identity");
    handles
        .process_env
        .publish_process_execution_env(
            &crate::ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
                crate::HostArtifactPin::mint(),
            ))
            .expect("host pin"),
            &env_ref,
            &spec.to_store_bytes().expect("encode environment"),
        )
        .await
        .expect("publish environment");
    handles
        .triggers
        .execute_command(
            "redelivery-matched-register",
            TriggerCommand::Register {
                owner_scope: owner(&session),
                actor: actor(&session),
                draft: TriggerSubscriptionDraft {
                    env_ref,
                    ..draft(
                        &session,
                        "redelivery-matched-key",
                        "redelivery-matched-source",
                    )
                },
            },
        )
        .await
        .expect("register call")
        .expect("register the subscription");
    let request = crate::TriggerOccurrenceRequest::new(
        SOURCE_TYPE,
        "redelivery-matched-source",
        serde_json::json!({ "button": "Blue" }),
        "redelivery-matched-occurrence",
    );

    let report = deliver(&handles, &request)
        .await
        .expect("the first delivery emits");
    assert_eq!(report.deliveries.len(), 1, "one subscription matches");
    let process_id = report.deliveries[0]
        .process_id
        .clone()
        .expect("the delivery started its process");

    // The process ends and is pruned; retention reclaims the delivery and
    // then the occurrence nothing references.
    handles
        .registry
        .complete_process(
            &process_id,
            ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                serde_json::Value::Null,
            )),
            ProcessCompletionAuthority::workflow_key(process_id.to_string()),
        )
        .await
        .expect("end the delivery's process");
    prune_with_trigger_cleanup(&handles).await;
    let reclaimed = held(&handles).await;
    assert_eq!(
        (reclaimed.occurrences.len(), reclaimed.deliveries.len()),
        (0, 0),
        "retention reclaimed the occurrence and its delivery"
    );
    assert!(
        matches!(
            handles.registry.get_process(&process_id).await,
            Err(crate::PluginError::ProcessNoLongerRetained { .. })
        ),
        "the delivery's process is pruned"
    );

    assert_redelivery_writes_nothing(&handles, &request, "matched").await;
    // The guard is idempotent: every further redelivery answers the same,
    // and no cutoff the host's reclaim pass names takes it away.
    assert_the_widest_cutoff_compacts_nothing(&handles.triggers, "matched").await;
    assert_redelivery_writes_nothing(&handles, &request, "matched, again").await;
}

/// An emission no subscription matched, reclaimed by the host's occurrence
/// reclaim pass. The pass's cutoff does not shorten the tombstone's life.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_redelivered_zero_match_emission_writes_no_reclaimed_occurrence_back(
    handles: ProcessTriggerRetentionHandles,
) {
    let request = crate::TriggerOccurrenceRequest::new(
        SOURCE_TYPE,
        "redelivery-zero-match-source",
        serde_json::json!({ "button": "Blue" }),
        "redelivery-zero-match-occurrence",
    );
    let report = deliver(&handles, &request)
        .await
        .expect("the first delivery emits");
    assert!(report.deliveries.is_empty(), "no subscription matches");
    let occurred_at_ms = held(&handles).await.occurrences[0].occurred_at_ms;

    // A cutoff at the occurrence's own instant reclaims it and leaves the
    // tombstone the reclaim writes, which is not older than the cutoff.
    let pass = handles
        .triggers
        .reclaim_trigger_occurrences(occurred_at_ms)
        .await
        .expect("reclaim the zero-match occurrence");
    assert_eq!(
        (
            pass.reclaimed_occurrence_count,
            pass.compacted_tombstone_count
        ),
        (1, 0),
        "the pass reclaimed the occurrence and kept its tombstone"
    );
    assert!(held(&handles).await.occurrences.is_empty());

    assert_redelivery_writes_nothing(&handles, &request, "zero-match").await;

    // A later pass whose cutoff is past the tombstone, and the same cutoff
    // on the pass that reclaims, leave the guard standing.
    assert_the_widest_cutoff_compacts_nothing(&handles.triggers, "zero-match").await;
    assert_redelivery_writes_nothing(&handles, &request, "zero-match, after a pass").await;

    let widest = crate::TriggerOccurrenceRequest::new(
        SOURCE_TYPE,
        "redelivery-zero-match-source",
        serde_json::json!({ "button": "Blue" }),
        "redelivery-zero-match-widest-cutoff-occurrence",
    );
    deliver(&handles, &widest)
        .await
        .expect("the first delivery emits");
    let pass = handles
        .triggers
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("reclaim at the widest cutoff");
    assert_eq!(
        (
            pass.reclaimed_occurrence_count,
            pass.compacted_tombstone_count
        ),
        (1, 0),
        "the widest cutoff reclaimed the occurrence and kept every tombstone"
    );
    assert_eq!(
        crate::store::MaintenanceReport::sweep(&pass),
        crate::store::MaintenanceSweep::Swept
    );
    assert_redelivery_writes_nothing(&handles, &widest, "zero-match, widest cutoff").await;
    assert_redelivery_writes_nothing(&handles, &request, "zero-match, both tombstones").await;
}

/// A non-fired audit occurrence the host pruned.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_redelivered_audit_emission_writes_no_pruned_occurrence_back(
    handles: ProcessTriggerRetentionHandles,
) {
    let request = crate::TriggerOccurrenceRequest::new(
        SOURCE_TYPE,
        "redelivery-audit-source",
        serde_json::json!({ "button": "Blue" }),
        "redelivery-audit-occurrence",
    )
    .with_outcome(crate::TriggerOccurrenceOutcome::Dropped {
        reason: "the schedule was paused".to_string(),
    });
    let report = deliver(&handles, &request)
        .await
        .expect("the first delivery records the audit row");
    assert!(
        report.deliveries.is_empty(),
        "a dropped occurrence fans out to nothing"
    );
    assert_eq!(held(&handles).await.occurrences.len(), 1);

    assert_eq!(
        handles
            .triggers
            .prune_non_fired_occurrences(u64::MAX)
            .await
            .expect("prune the audit row"),
        1
    );
    assert!(held(&handles).await.occurrences.is_empty());

    assert_redelivery_writes_nothing(&handles, &request, "audit").await;
    assert_the_widest_cutoff_compacts_nothing(&handles.triggers, "audit").await;
    assert_redelivery_writes_nothing(&handles, &request, "audit, after a pass").await;
}

/// A tombstone lasts the redelivery horizon whatever cutoff the reclaim pass
/// names, and a cutoff can only keep it longer. `make` opens a trigger store
/// on the clock it is given.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_tombstone_outlives_the_redelivery_horizon_whatever_the_cutoff<F, Fut>(make: F)
where
    F: Fn(Arc<dyn crate::Clock>) -> Fut,
    Fut: Future<Output = Arc<dyn TriggerStore>>,
{
    const RECLAIMED_AT_MS: u64 = 4_000_000_000_000;
    let clock = Arc::new(crate::testing::TestClock::new(RECLAIMED_AT_MS));
    let triggers = make(Arc::clone(&clock) as Arc<dyn crate::Clock>).await;
    let request = crate::TriggerOccurrenceRequest::new(
        SOURCE_TYPE,
        "redelivery-horizon-source",
        serde_json::json!({ "button": "Blue" }),
        "redelivery-horizon-occurrence",
    );
    let passes = |cutoff_epoch_ms: u64| {
        let triggers = Arc::clone(&triggers);
        async move {
            let pass = triggers
                .reclaim_trigger_occurrences(cutoff_epoch_ms)
                .await
                .expect("run the reclaim pass");
            (
                pass.reclaimed_occurrence_count,
                pass.compacted_tombstone_count,
            )
        }
    };
    let assert_refused = |case: &'static str| {
        let triggers = Arc::clone(&triggers);
        let request = request.clone();
        async move {
            match triggers.ingest_occurrence(request).await {
                Err(error) => assert!(
                    crate::is_trigger_occurrence_reclaimed(&error),
                    "{case}: the redelivery is refused as reclaimed, got {error:?}"
                ),
                Ok(receipt) => panic!("{case}: the redelivery was ingested again: {receipt:?}"),
            }
            assert!(
                triggers
                    .list_occurrences(crate::TriggerOccurrenceFilter::default())
                    .await
                    .expect("list occurrences")
                    .is_empty(),
                "{case}: the redelivery wrote the occurrence back"
            );
        }
    };

    triggers
        .ingest_occurrence(request.clone())
        .await
        .expect("the first delivery records the occurrence");
    assert_eq!(
        passes(u64::MAX).await,
        (1, 0),
        "the pass that reclaims keeps the tombstone it writes"
    );
    assert_refused("at the reclaim").await;

    // The horizon's last instant: the tombstone is exactly as old as the
    // horizon, and the widest cutoff still leaves it.
    clock.advance(crate::TRIGGER_OCCURRENCE_REDELIVERY_HORIZON_MS);
    assert_eq!(passes(u64::MAX).await, (0, 0), "inside the horizon");
    assert_refused("at the horizon's last instant").await;

    // Past the horizon a cutoff still defers: one at the tombstone's own
    // instant keeps it.
    clock.advance(1);
    assert_eq!(
        passes(RECLAIMED_AT_MS).await,
        (0, 0),
        "a cutoff keeps a tombstone longer than the horizon"
    );
    assert_refused("past the horizon, under an earlier cutoff").await;

    // Past both, the pass compacts it, and the identity is a new emission.
    assert_eq!(
        passes(u64::MAX).await,
        (0, 1),
        "past the horizon and the cutoff"
    );
    triggers
        .ingest_occurrence(request)
        .await
        .expect("an emission past the horizon records a new occurrence");
    assert_eq!(
        triggers
            .list_occurrences(crate::TriggerOccurrenceFilter::default())
            .await
            .expect("list occurrences")
            .len(),
        1
    );
}
