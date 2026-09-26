//! Single realization across two invocations on the controller-owned
//! (ordinal-addressed) tool-intent tier (FIG-1489, FIG-3072).

use super::*;

// ---------------------------------------------------------------------------
// FIG-1489: single realization across two invocations on a controller-owned
// (ordinal-addressed) tier, for all five submitted intent shapes.
//
// Within one invocation a controller-owned tier already dedupes: the effect
// journal replays the recorded outcome, which is what
// `duplicate_host_submit_returns_the_same_outcome_and_realizes_once` proves.
// That is not the redelivery case. A redelivery after a crash arrives on a new
// invocation with an empty journal, so the submission reaches the store, and
// the only thing that can fence it there is the durable key the shape lands on
// at the point it mutates.
//
// Each test below therefore submits from a *second* core over the same durable
// registry (or trigger store), and asserts two facts: the identity realizes
// once, and the same identity carrying different content is refused as
// `DuplicateIdentity` — the same typed refusal the runtime-owned tier returns
// from its submission ledger, so hosts see one vocabulary on both tiers.
// ---------------------------------------------------------------------------

fn ingress_of(core: &LashCore) -> Result<crate::tools::ToolIntentIngress> {
    core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))
}

fn signal_intent(session_id: &SessionId, process: &ProcessId) -> lash_core::ToolIntent {
    lash_core::ToolIntent::SignalProcess(lash_core::SignalProcessIntent {
        session_id: SessionId::from(session_id.to_string()),
        process_id: process.clone(),
        signal_name: SIGNAL.to_string(),
        payload: serde_json::json!({"law": "redelivered-signal"}),
    })
}

fn assert_admitted(outcome: &crate::tools::ToolIntentIngressOutcome, context: &str) {
    assert!(
        matches!(
            outcome,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                outcome: lash_core::ToolIntentExecutionOutcome::Executed { .. },
                ..
            }
        ),
        "{context} must be admitted, got {outcome:?}"
    );
}

/// Assert an admitted outcome reports the expected `replayed` verdict.
fn assert_replayed(
    outcome: &crate::tools::ToolIntentIngressOutcome,
    expected: bool,
    context: &str,
) {
    match outcome {
        crate::tools::ToolIntentIngressOutcome::Admitted { replayed, .. } => assert_eq!(
            *replayed, expected,
            "{context} must report replayed: {expected}, got {outcome:?}"
        ),
        other => panic!("{context} must be admitted, got {other:?}"),
    }
}

fn assert_duplicate_identity(
    outcome: &crate::tools::ToolIntentIngressOutcome,
    expected: lash_core::ToolIntentKind,
) {
    match outcome {
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity { kind },
        } => assert_eq!(*kind, expected, "the refusal names the submitted kind"),
        other => panic!(
            "a bound identity carrying different content must be refused as DuplicateIdentity, \
             got {other:?}"
        ),
    }
}

#[tokio::test]
async fn redelivered_start_realizes_one_process_and_a_changed_declaration_returns_it() -> Result<()>
{
    let (core, registry, _process) = ingress_core(memory_store_backend().await).await?;
    let key = ingress_of(&core)?.key("redelivered-start", 0);

    let first = ingress_of(&core)?
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
    assert_admitted(&first, "the first start");
    let started = super::started_process_id(&first);
    let created_at = registry
        .get_process(&started)
        .await?
        .expect("the start realizes a process")
        .created_at_ms;

    let redelivery = second_invocation_of(&core).await?;
    let replayed = ingress_of(&redelivery)?
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
    assert_admitted(&replayed, "the redelivered start");
    assert_replayed(
        &replayed,
        true,
        "a start the registry coalesced onto the recorded row",
    );
    assert_eq!(
        super::started_process_id(&replayed),
        started,
        "the redelivery answers the process its key minted"
    );
    assert_eq!(
        registry
            .get_process(&started)
            .await?
            .expect("the coalesced start returns the original process")
            .created_at_ms,
        created_at,
        "the start key derived from the identity coalesces the redelivery onto one process"
    );

    // The start key is trusted (ADR 0107): a changed declaration under the
    // same identity names the same key, so the registry returns the process
    // the key first minted and writes nothing.
    let changed_invocation = second_invocation_of(&core).await?;
    let mut changed = start_intent(&SessionId::from(SESSION));
    let lash_core::ToolIntent::StartProcess(intent) = &mut changed else {
        unreachable!("fixture is a start intent")
    };
    intent.declaration.input = lash_core::ProcessInput::External {
        metadata: serde_json::json!({"law": "changed-under-a-bound-identity"}),
    };
    let coalesced = ingress_of(&changed_invocation)?.submit(key, changed).await;
    assert_replayed(&coalesced, true, "a changed declaration under a bound key");
    assert_eq!(
        super::started_process_id(&coalesced),
        started,
        "the key answers the process it first minted"
    );
    let retained = registry
        .get_process(&started)
        .await?
        .expect("the first process stays in place");
    assert_eq!(retained.created_at_ms, created_at);
    assert_eq!(
        retained.input.as_ref(),
        &lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        "the changed declaration never reaches the recorded row"
    );
    Ok(())
}

/// The three dispositions of one controller-owned start, told apart by the
/// `replayed` bit the host reads (FIG-3070).
///
/// A fresh submission realizes. A submission the *effect journal* already holds
/// replays without reaching the store. A submission on a fresh journal whose
/// registration the *store* already holds coalesces: local execution runs, the
/// registry returns the recorded row, and nothing is written. The third used to
/// report `replayed: false`, because the ingress read "did local execution run"
/// rather than the store's verdict.
#[tokio::test]
async fn a_coalesced_start_reports_replayed_and_a_fresh_start_does_not() -> Result<()> {
    let (core, registry, _process) = ingress_core(memory_store_backend().await).await?;
    let key = ingress_of(&core)?.key("coalesced-start", 0);

    let realized = ingress_of(&core)?
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
    assert_replayed(&realized, false, "the start that created the row");
    let started = super::started_process_id(&realized);
    let created_at = registry
        .get_process(&started)
        .await?
        .expect("the start realizes a process")
        .created_at_ms;

    // Same invocation, so the controller's effect journal answers before the
    // store is consulted at all.
    let journal_replay = ingress_of(&core)?
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
    assert_replayed(&journal_replay, true, "a start the journal replayed");

    // A fresh invocation carries a fresh journal, so this one reaches the
    // registry, which coalesces it onto the row the first start created. The
    // durable key, not the journal, is what makes it a replay.
    let redelivery = second_invocation_of(&core).await?;
    let coalesced = ingress_of(&redelivery)?
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
    assert_replayed(&coalesced, true, "a start the registry coalesced");
    assert_eq!(
        registry
            .get_process(&started)
            .await?
            .expect("the coalesced start returns the original process")
            .created_at_ms,
        created_at,
        "all three submissions name the one process the first start created"
    );
    Ok(())
}

#[tokio::test]
async fn redelivered_event_appends_once_and_refuses_a_changed_payload() -> Result<()> {
    let (core, registry, process) = ingress_core(memory_store_backend().await).await?;
    let key = ingress_of(&core)?.key("redelivered-emit", 0);

    let first = ingress_of(&core)?
        .submit(
            key.clone(),
            emit_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    assert_admitted(&first, "the first emission");

    let redelivery = second_invocation_of(&core).await?;
    let replayed = ingress_of(&redelivery)?
        .submit(
            key.clone(),
            emit_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    assert_admitted(&replayed, "the redelivered emission");
    assert_replayed(
        &replayed,
        true,
        "an append the store coalesced onto the recorded event",
    );
    assert_eq!(
        emitted_event_count(&registry, &process, EVENT).await?,
        1,
        "the event replay key coalesces the redelivery onto the first append"
    );

    let changed_invocation = second_invocation_of(&core).await?;
    let mut changed = emit_intent(&SessionId::from(SESSION), &process);
    let lash_core::ToolIntent::EmitProcessEvent(intent) = &mut changed else {
        unreachable!("fixture is an event intent")
    };
    intent.payload = serde_json::json!({"law": "changed-under-a-bound-key"});
    let refused = ingress_of(&changed_invocation)?.submit(key, changed).await;
    assert_duplicate_identity(&refused, lash_core::ToolIntentKind::EmitProcessEvent);
    assert_eq!(
        emitted_event_count(&registry, &process, EVENT).await?,
        1,
        "a refused change cannot append a second event"
    );
    Ok(())
}

#[tokio::test]
async fn redelivered_signal_appends_once_and_refuses_a_changed_payload() -> Result<()> {
    let (core, registry, process) = ingress_core(memory_store_backend().await).await?;
    let key = ingress_of(&core)?.key("redelivered-signal", 0);
    let signal_event = format!("signal.{SIGNAL}");

    let first = ingress_of(&core)?
        .submit(
            key.clone(),
            signal_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    assert_admitted(&first, "the first signal");

    let redelivery = second_invocation_of(&core).await?;
    let replayed = ingress_of(&redelivery)?
        .submit(
            key.clone(),
            signal_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    assert_admitted(&replayed, "the redelivered signal");
    assert_replayed(
        &replayed,
        true,
        "a signal the store coalesced onto the recorded event",
    );
    assert_eq!(
        emitted_event_count(&registry, &process, &signal_event).await?,
        1,
        "the signal wait key coalesces the redelivery onto the first append"
    );

    let changed_invocation = second_invocation_of(&core).await?;
    let mut changed = signal_intent(&SessionId::from(SESSION), &process);
    let lash_core::ToolIntent::SignalProcess(intent) = &mut changed else {
        unreachable!("fixture is a signal intent")
    };
    intent.payload = serde_json::json!({"law": "changed-under-a-bound-key"});
    let refused = ingress_of(&changed_invocation)?.submit(key, changed).await;
    assert_duplicate_identity(&refused, lash_core::ToolIntentKind::SignalProcess);
    assert_eq!(
        emitted_event_count(&registry, &process, &signal_event).await?,
        1,
        "a refused change cannot append a second signal"
    );
    Ok(())
}

#[tokio::test]
async fn redelivered_cancel_requests_the_same_cancellation_once() -> Result<()> {
    let (core, registry, process) = ingress_core(memory_store_backend().await).await?;
    let key = ingress_of(&core)?.key("redelivered-cancel", 0);

    let first = ingress_of(&core)?
        .submit(
            key.clone(),
            cancel_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    assert_admitted(&first, "the first cancel");
    let requested = cancel_request_snapshot(&registry, &process).await?;
    assert!(
        requested.is_some(),
        "the first cancel records a cancel request"
    );

    let redelivery = second_invocation_of(&core).await?;
    let replayed = ingress_of(&redelivery)?
        .submit(
            key.clone(),
            cancel_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    assert_admitted(&replayed, "the redelivered cancel");
    assert_replayed(
        &replayed,
        true,
        "a cancel the store coalesced onto the recorded request",
    );
    assert_eq!(
        cancel_request_snapshot(&registry, &process).await?,
        requested,
        "the identity is the cancel requester, so the redelivery coalesces onto the first request"
    );

    // A cancel intent carries no payload beyond its target, so "changed
    // content under the same identity" can only mean a different target. The
    // cancel fence lives on the target record (first writer wins, matching
    // requester replays), so a second target is unfenced and nothing on it can
    // see the first binding. The pre-realization submission ledger is what
    // binds the identity to the target it first named (FIG-3072), so the
    // changed target is refused with the same `DuplicateIdentity` vocabulary
    // the other four shapes and the runtime-owned tier use, and the second
    // process is never cancelled.
    let other = registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::RecoveryContract::ExternallyOwned,
                lash_core::ProcessProvenance::host(),
                lash_core::ProcessLifecyclePolicy::new(
                    lash_core::ParentScope::Host,
                    lash_core::OnParentEnd::Abandon,
                ),
            ),
            &[SessionId::from(SESSION)],
        )
        .await?;
    assert!(other.cancel_request.is_none());

    let changed_invocation = second_invocation_of(&core).await?;
    let refused = ingress_of(&changed_invocation)?
        .submit(
            key,
            cancel_intent_for_target(&SessionId::from(SESSION), &other.id),
        )
        .await;
    assert_duplicate_identity(&refused, lash_core::ToolIntentKind::CancelProcess);
    assert!(
        cancel_request_snapshot(&registry, &other.id)
            .await?
            .is_none(),
        "the refused change never reaches the second target"
    );
    assert_eq!(
        cancel_request_snapshot(&registry, &process).await?,
        requested,
        "the refused change leaves the first cancellation as recorded"
    );
    Ok(())
}

#[tokio::test]
async fn redelivered_trigger_ingests_once_and_refuses_a_changed_payload() -> Result<()> {
    let (core, store, _subscription, _registry) = ingress_core_with_trigger_store(
        memory_store_backend().await,
        Arc::new(KeyJournalController::default()),
    )
    .await?;
    let key = ingress_of(&core)?.key("redelivered-trigger", 0);

    let first = ingress_of(&core)?
        .submit(key.clone(), trigger_intent(&SessionId::from(SESSION)))
        .await;
    assert_admitted(&first, "the first emission");

    let redelivery = second_invocation_of(&core).await?;
    let replayed = ingress_of(&redelivery)?
        .submit(key.clone(), trigger_intent(&SessionId::from(SESSION)))
        .await;
    assert_admitted(&replayed, "the redelivered emission");
    assert_replayed(
        &replayed,
        true,
        "an occurrence the trigger store coalesced onto the recorded one",
    );
    assert_eq!(
        store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await?
            .len(),
        1,
        "the occurrence idempotency key coalesces the redelivery onto the first ingest"
    );

    let changed_invocation = second_invocation_of(&core).await?;
    let mut changed = trigger_intent(&SessionId::from(SESSION));
    let lash_core::ToolIntent::EmitTrigger(intent) = &mut changed else {
        unreachable!("fixture is a trigger intent")
    };
    intent.request.payload = serde_json::json!({"law": "changed-under-a-bound-key"});
    let refused = ingress_of(&changed_invocation)?.submit(key, changed).await;
    assert_duplicate_identity(&refused, lash_core::ToolIntentKind::EmitTrigger);
    assert_eq!(
        store
            .list_occurrences(lash_core::TriggerOccurrenceFilter::default())
            .await?
            .len(),
        1,
        "a refused change cannot ingest a second occurrence"
    );
    Ok(())
}

async fn emitted_event_count(
    registry: &Arc<dyn ProcessRegistry>,
    process: &ProcessId,
    event_type: &str,
) -> Result<usize> {
    Ok(registry
        .full_event_window(process, 0)
        .await?
        .iter()
        .filter(|event| event.event_type == event_type)
        .count())
}

async fn cancel_request_snapshot(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Result<Option<lash_core::CancelRequest>> {
    Ok(registry
        .get_process(process_id)
        .await?
        .expect("the fixture process exists")
        .cancel_request
        .map(|request| *request))
}
