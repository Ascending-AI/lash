//! A redelivered emission on a host that journals nothing never writes a
//! reclaimed occurrence or delivery back (FIG-4513).
//!
//! A host with no journal runs an emission's ingest again on every
//! redelivery. Once retention has reclaimed the occurrence, the ingest finds
//! no row under the idempotency key. The trigger store answers from the
//! tombstone the reclaim left: it refuses the ingest as reclaimed and writes
//! nothing, so the emission reserves and starts nothing.
//!
//! Reclaim never deletes the tombstone (FIG-4610). Only an explicit host
//! forget allows its identity to run again.

use super::*;
use crate::ClockWallTime as _;
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

/// The widest reclaim cutoff retains the redelivery fence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_the_widest_cutoff_reclaims_nothing(triggers: &Arc<dyn TriggerStore>, case: &str) {
    let pass = triggers
        .reclaim_trigger_occurrences(u64::MAX)
        .await
        .expect("run the reclaim pass");
    assert_eq!(
        pass.reclaimed_occurrence_count, 0,
        "{case}: no occurrence remains to reclaim"
    );
}

/// A matched emission whose process was pruned, and whose occurrence and
/// delivery retention then reclaimed.
pub(super) async fn a_redelivered_emission_writes_no_reclaimed_delivery_back(
    handles: ProcessTriggerRetentionHandles,
) {
    let request = reclaimed_matched_occurrence(&handles, "redelivery-matched-occurrence").await;
    assert_redelivery_writes_nothing(&handles, &request, "matched").await;
    assert_the_widest_cutoff_reclaims_nothing(&handles.triggers, "matched").await;
    assert_redelivery_writes_nothing(&handles, &request, "matched, again").await;
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn reclaimed_matched_occurrence(
    handles: &ProcessTriggerRetentionHandles,
    occurrence_key: &str,
) -> crate::TriggerOccurrenceRequest {
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
        occurrence_key,
    );

    let report = deliver(handles, &request)
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
    prune_with_trigger_cleanup(handles).await;
    let reclaimed = held(handles).await;
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

    request
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
        pass.reclaimed_occurrence_count, 1,
        "the pass reclaimed the occurrence and kept its tombstone"
    );
    assert!(held(&handles).await.occurrences.is_empty());

    assert_redelivery_writes_nothing(&handles, &request, "zero-match").await;

    // A later pass whose cutoff is past the tombstone, and the same cutoff
    // on the pass that reclaims, leave the guard standing.
    assert_the_widest_cutoff_reclaims_nothing(&handles.triggers, "zero-match").await;
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
        pass.reclaimed_occurrence_count, 1,
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
    assert_the_widest_cutoff_reclaims_nothing(&handles.triggers, "audit").await;
    assert_redelivery_writes_nothing(&handles, &request, "audit, after a pass").await;
}

/// Even decades past the former expiry, every cutoff retains the fence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn tombstones_survive_every_reclaim<F, Fut>(make: F)
where
    F: Fn(Arc<dyn crate::Clock>) -> Fut,
    Fut: Future<Output = Arc<dyn TriggerStore>>,
{
    const RECLAIMED_AT_MS: u64 = 4_000_000_000_000;
    let clock = Arc::new(crate::testing::TestClock::new(RECLAIMED_AT_MS));
    let triggers = make(Arc::clone(&clock) as Arc<dyn crate::Clock>).await;
    let request = crate::TriggerOccurrenceRequest::new(
        SOURCE_TYPE,
        "retained-source",
        serde_json::json!({ "button": "Blue" }),
        "retained-occurrence",
    );
    triggers
        .ingest_occurrence(request.clone())
        .await
        .expect("ingest first occurrence");
    assert_eq!(
        triggers
            .reclaim_trigger_occurrences(u64::MAX)
            .await
            .expect("reclaim occurrence")
            .reclaimed_occurrence_count,
        1
    );
    for advance in [
        0,
        7 * 24 * 60 * 60 * 1000 + 1,
        100 * 365 * 24 * 60 * 60 * 1000,
    ] {
        clock.advance(advance);
        for cutoff in [0, RECLAIMED_AT_MS, clock.timestamp_ms(), u64::MAX] {
            assert_eq!(
                triggers
                    .reclaim_trigger_occurrences(cutoff)
                    .await
                    .expect("reclaim at every age and cutoff")
                    .reclaimed_occurrence_count,
                0
            );
            let error = triggers
                .ingest_occurrence(request.clone())
                .await
                .expect_err("every reclaim retains the redelivery fence");
            assert!(
                crate::is_trigger_occurrence_reclaimed(&error),
                "typed refusal at age/cutoff {advance}/{cutoff}: {error:?}"
            );
            assert!(
                triggers
                    .list_occurrences(crate::TriggerOccurrenceFilter::default())
                    .await
                    .expect("list occurrences")
                    .is_empty()
            );
            assert!(
                triggers
                    .list_deliveries()
                    .await
                    .expect("list deliveries")
                    .is_empty()
            );
        }
    }
}

/// Forgetting is an exclusive cutoff on the tombstone's write time, not the
/// occurrence's ingest time. Rows at the cutoff survive and counts are exact.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn forgetting_selects_exactly_the_tombstones_written_before_the_cutoff<F, Fut>(
    make: F,
) where
    F: Fn(Arc<dyn crate::Clock>) -> Fut,
    Fut: Future<Output = Arc<dyn TriggerStore>>,
{
    const INGESTED_AT_MS: u64 = 4_000_000_000_000;
    let clock = Arc::new(crate::testing::TestClock::new(INGESTED_AT_MS));
    let triggers = make(Arc::clone(&clock) as Arc<dyn crate::Clock>).await;
    let requests = ["older-one", "older-two", "at-cutoff"].map(|key| {
        crate::TriggerOccurrenceRequest::new(
            SOURCE_TYPE,
            "forget-source",
            serde_json::json!({ "button": "Blue" }),
            key,
        )
    });
    for request in &requests[..2] {
        triggers
            .ingest_occurrence(request.clone())
            .await
            .expect("ingest older occurrence");
    }
    clock.advance(10);
    assert_eq!(
        triggers
            .reclaim_trigger_occurrences(u64::MAX)
            .await
            .expect("write older tombstones")
            .reclaimed_occurrence_count,
        2
    );
    triggers
        .ingest_occurrence(requests[2].clone())
        .await
        .expect("ingest newer occurrence");
    clock.advance(10);
    assert_eq!(
        triggers
            .reclaim_trigger_occurrences(u64::MAX)
            .await
            .expect("write newer tombstone")
            .reclaimed_occurrence_count,
        1
    );
    assert_eq!(
        triggers
            .forget_trigger_tombstones(0)
            .await
            .expect("empty cutoff"),
        0
    );
    assert_eq!(
        triggers
            .forget_trigger_tombstones(INGESTED_AT_MS + 10)
            .await
            .expect("exclusive older write time"),
        0
    );
    assert_eq!(
        triggers
            .forget_trigger_tombstones(INGESTED_AT_MS + 20)
            .await
            .expect("forget exactly older writes"),
        2
    );
    assert_eq!(
        triggers
            .forget_trigger_tombstones(INGESTED_AT_MS + 20)
            .await
            .expect("repeat forget"),
        0
    );
    for request in &requests[..2] {
        triggers
            .ingest_occurrence(request.clone())
            .await
            .expect("forgotten identities ingest again");
    }
    let error = triggers
        .ingest_occurrence(requests[2].clone())
        .await
        .expect_err("the tombstone at the cutoff survives");
    assert!(
        crate::is_trigger_occurrence_reclaimed(&error),
        "typed retained refusal: {error:?}"
    );
    assert_eq!(
        triggers
            .forget_trigger_tombstones(u64::MAX)
            .await
            .expect("forget remaining tombstone"),
        1
    );
    let max_clock_ms = chrono::DateTime::<chrono::Utc>::MAX_UTC.timestamp_millis() as u64;
    clock.set(max_clock_ms);
    assert_eq!(
        triggers
            .reclaim_trigger_occurrences(u64::MAX)
            .await
            .expect("write at greatest clock timestamp")
            .reclaimed_occurrence_count,
        2
    );
    assert_eq!(
        triggers
            .forget_trigger_tombstones(max_clock_ms)
            .await
            .expect("exclusive greatest clock timestamp"),
        0
    );
    assert_eq!(
        triggers
            .forget_trigger_tombstones(u64::MAX)
            .await
            .expect("forget greatest clock timestamp"),
        2
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_forgotten_redelivery_starts_again_while_a_retained_one_is_refused(
    handles: ProcessTriggerRetentionHandles,
) {
    let forgotten = reclaimed_matched_occurrence(&handles, "forgotten-matched-occurrence").await;
    assert_redelivery_writes_nothing(&handles, &forgotten, "before forget").await;
    assert_eq!(
        handles
            .triggers
            .forget_trigger_tombstones(u64::MAX)
            .await
            .expect("host forgets first occurrence"),
        1
    );
    let retained = reclaimed_matched_occurrence(&handles, "retained-matched-occurrence").await;
    assert_redelivery_writes_nothing(&handles, &retained, "retained tombstone").await;
    let before = held(&handles).await;
    let report = deliver(&handles, &forgotten)
        .await
        .expect("forgotten redelivery emits again");
    assert_eq!(report.deliveries.len(), 1);
    assert!(
        report.deliveries[0].process_id.is_some(),
        "the forgotten redelivery starts a process"
    );
    let after = held(&handles).await;
    assert_eq!(after.processes, before.processes + 1);
    assert_eq!(after.occurrences.len(), before.occurrences.len() + 1);
    assert_eq!(after.deliveries.len(), before.deliveries.len() + 1);
    assert_redelivery_writes_nothing(&handles, &retained, "retained after forgotten redelivery")
        .await;
}
