//! FIG-4829: the first writer's trace provenance is retained.
//!
//! An admitted operation retains what caused it, and where it parents later
//! work the anchor its first admission selected, in the write that inserts
//! its owning row. A retry of the same business submission under another
//! trace context is the same submission: it reads the retained provenance
//! back, changes no hash or key, and turns no retry into a conflict. A
//! changed submission is refused exactly as it was before any provenance
//! existed. An admission that finds its row already there reports an
//! existing scope, which holds no emission permit, so nothing is dispatched
//! or emitted under an anchor that lost.
//!
//! The law walks every owning row through the stores under test: a turn
//! input, the run that admits it, a queued process wake, a process start, a
//! signal, a trigger occurrence and a host-submitted tool intent.

use std::sync::Arc;

use lash_core::store::AdmittedHead;
use lash_core::testing::store_fixtures::{admit_run_request_for_test, seal_shift_fence_for_test};
use lash_core::{
    DurableTraceScope, TraceAnchor, TraceCarrier, TraceCause, TraceScopeAdmission, TraceScopeId,
    TraceScopeOffer, TraceScopeOwner,
};
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

use super::shift_admission::ShiftParts;

/// The context of producer `producer`: a sampled span of its own trace.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the header is well formed"
)]
fn context(producer: u8) -> TraceCarrier {
    TraceCarrier::parse_w3c(
        &format!("00-{producer:032x}-{producer:016x}-01"),
        Some("vendor=first"),
    )
    .expect("a valid trace context")
}

fn linked(producer: u8) -> TraceCause {
    TraceCause::linked_to(Some(context(producer)))
}

/// The offer of an admission caused by `producer` whose candidate proposed
/// the anchor `candidate`.
fn offer(producer: u8, candidate: u8) -> TraceScopeOffer {
    TraceScopeOffer::new(linked(producer), TraceAnchor::Context(context(candidate)))
}

const FIRST: u8 = 1;
const SECOND: u8 = 2;
const FIRST_ANCHOR: u8 = 0xa1;
const SECOND_ANCHOR: u8 = 0xa2;

/// Law P2: the first admission's cause and anchor are retained across
/// retried inputs, runs, queued wakes, process starts, signals, trigger
/// occurrences and host tool intents, without changing a business hash, key
/// or typed conflict, and a retained read yields no emission permit.
pub async fn first_admission_wins_without_changing_business_identity(
    prefix: &str,
    host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    _: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "trace-first-writer", &host, &stores, 4).await;
    a_retried_input_keeps_its_first_cause(&parts).await;
    a_run_keeps_the_anchor_its_first_admission_offered(&parts).await;
    a_redelivered_wake_keeps_its_first_cause(&parts).await;
    a_retried_start_keeps_its_first_scope(prefix, &stores).await;
    a_redelivered_signal_keeps_its_first_cause(prefix, &stores).await;
    a_redelivered_occurrence_keeps_its_first_scope(prefix, &stores).await;
    a_resubmitted_intent_keeps_its_first_scope(&parts, &stores).await;
}

fn input_draft(
    parts: &ShiftParts,
    key: &str,
    text: &str,
    cause: TraceCause,
) -> crate::PendingTurnInputDraft {
    crate::PendingTurnInputDraft::new(
        parts.session_id.clone(),
        crate::TurnInputIngress::next_turn(),
        crate::TurnInput::text(text),
    )
    .with_source_key(key)
    .with_trace_cause(cause)
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_retried_input_keeps_its_first_cause(parts: &ShiftParts) {
    let draft = |cause| input_draft(parts, "traced-run", "the accepted words", cause);
    let digest = |draft: crate::PendingTurnInputDraft| {
        draft
            .submission_digest()
            .expect("the submission has a digest")
    };
    assert_eq!(
        digest(draft(linked(FIRST))),
        digest(draft(TraceCause::Root)),
        "the cause is no part of the submission"
    );
    assert_eq!(digest(draft(linked(FIRST))), digest(draft(linked(SECOND))));

    let accepted = parts
        .store
        .enqueue_pending_turn_input(draft(linked(FIRST)))
        .await
        .expect("the first acceptance");
    assert_eq!(accepted.trace_cause, linked(FIRST));
    for retry in [linked(SECOND), TraceCause::Root] {
        let retried = parts
            .store
            .enqueue_pending_turn_input(draft(retry))
            .await
            .expect("a retry under another context is the same submission");
        assert_eq!(retried.input_id, accepted.input_id);
        assert_eq!(
            retried.trace_cause,
            linked(FIRST),
            "a retry reads the first cause back"
        );
    }
    let listed = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("list the pending inputs");
    assert_eq!(
        listed
            .iter()
            .map(|read| read.input.trace_cause.clone())
            .collect::<Vec<_>>(),
        vec![linked(FIRST)]
    );

    // A changed submission is the typed conflict it always was.
    match parts
        .store
        .enqueue_pending_turn_input(input_draft(
            parts,
            "traced-run",
            "other words",
            linked(FIRST),
        ))
        .await
    {
        Err(crate::StoreError::PendingTurnInputSourceKeyConflict { .. }) => {}
        answer => panic!("changed content under the key is refused typed: {answer:?}"),
    }

    // Absence is retained too: an untraced acceptance stays untraced.
    let untraced = |cause| input_draft(parts, "untraced-run", "words nobody traced", cause);
    let accepted = parts
        .store
        .enqueue_pending_turn_input(untraced(TraceCause::Root))
        .await
        .expect("the untraced acceptance");
    assert_eq!(accepted.trace_cause, TraceCause::Root);
    let retried = parts
        .store
        .enqueue_pending_turn_input(untraced(linked(SECOND)))
        .await
        .expect("a traced retry of an untraced submission");
    assert_eq!(retried.input_id, accepted.input_id);
    assert_eq!(
        retried.trace_cause,
        TraceCause::Root,
        "a retry cannot fill in a cause the first acceptance lacked"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_run_keeps_the_anchor_its_first_admission_offered(parts: &ShiftParts) {
    let run = TurnId::from("traced-run");
    let head = parts
        .store
        .list_pending_turn_inputs(&parts.session_id)
        .await
        .expect("list the pending inputs")
        .into_iter()
        .find(|read| read.input.source_key.as_deref() == Some("traced-run"))
        .expect("the traced input is pending")
        .input
        .input_id;
    let request = |fence: &crate::store::ShiftFence, candidate: u8| {
        let mut request =
            admit_run_request_for_test(fence, &run, AdmittedHead::Input(head.clone()));
        // One input per run, so the run's cause is its head's.
        request.max_inputs = 1;
        request.trace_anchor = TraceAnchor::Context(context(candidate));
        request
    };
    let first = seal_shift_fence_for_test(&parts.store, &parts.session_id, "first").await;
    let won = parts
        .store
        .admit_run(&request(&first, FIRST_ANCHOR))
        .await
        .expect("the first admission records the run")
        .expect("the first admission reaches its head");
    let scope = won.trace.clone().expect("the admission retains a scope");
    assert_eq!(
        scope.scope,
        TraceScopeId::admission(TraceScopeOwner::Run {
            session_id: parts.session_id.clone(),
            run: run.clone(),
        })
    );
    assert_eq!(
        scope.cause,
        linked(FIRST),
        "the run's cause is what caused the input it admitted"
    );
    assert_eq!(scope.anchor, TraceAnchor::Context(context(FIRST_ANCHOR)));
    let admission = won.trace_admission().expect("the admission's receipt");
    assert_eq!(admission, TraceScopeAdmission::Inserted(scope.clone()));
    assert!(
        admission.permit().is_some(),
        "the admission that recorded the run may emit it"
    );

    // A competing admission, under a later fence and another candidate, is
    // answered the winner: it dispatches under no anchor of its own.
    let later = seal_shift_fence_for_test(&parts.store, &parts.session_id, "later").await;
    for fence in [&first, &later] {
        let Ok(lost) = parts.store.admit_run(&request(fence, SECOND_ANCHOR)).await else {
            // A superseded fence is refused before anything is read.
            continue;
        };
        let lost = lost.expect("the recorded admission is read back");
        let admission = lost.trace_admission().expect("the retained scope");
        assert_eq!(admission, TraceScopeAdmission::Existing(scope.clone()));
        assert!(
            admission.permit().is_none(),
            "a retained read holds no emission permit"
        );
    }

    // A worker that died after the commit and before its journal took the
    // outcome leaves a successor the recorded admission: read back from a
    // journal or from the row, it is retained, never fresh.
    let journaled: crate::store::RunAdmission =
        serde_json::from_value(serde_json::to_value(&won).expect("journal the admission"))
            .expect("replay the journaled admission");
    assert_eq!(
        journaled.trace_admission(),
        Some(TraceScopeAdmission::Existing(scope))
    );
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_redelivered_wake_keeps_its_first_cause(parts: &ShiftParts) {
    let wake = |cause| {
        super::process_wake_work(
            &parts.session_id,
            "trace-wake",
            1,
            "the process woke the session",
            crate::DeliveryPolicy::EarliestSafeBoundary,
        )
        .with_trace_cause(cause)
    };
    assert_eq!(
        wake(linked(FIRST))
            .submission_digest()
            .expect("the wake has a digest"),
        wake(TraceCause::Root)
            .submission_digest()
            .expect("the wake has a digest"),
        "the cause is no part of the wake's submission"
    );
    let first = parts
        .store
        .enqueue_queued_work_with_outcome(wake(linked(FIRST)))
        .await
        .expect("the first delivery");
    let crate::QueuedWorkEnqueueOutcome::Inserted(inserted) = first else {
        panic!("the first delivery inserts the wake: {first:?}");
    };
    assert_eq!(inserted.trace_cause, linked(FIRST));
    let again = parts
        .store
        .enqueue_queued_work_with_outcome(wake(linked(SECOND)))
        .await
        .expect("a redelivery under another context is absorbed");
    let crate::QueuedWorkEnqueueOutcome::Existing(existing) = again else {
        panic!("a redelivery is absorbed: {again:?}");
    };
    assert_eq!(existing.batch_id, inserted.batch_id);
    assert_eq!(existing.trace_cause, linked(FIRST));
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_retried_start_keeps_its_first_scope(prefix: &str, stores: &Arc<dyn crate::StoreSet>) {
    let registry = stores.process_registry();
    let key = crate::StartKey::for_host(format!("{prefix}-trace-start"));
    let start = |offer: TraceScopeOffer| {
        super::process_registry::registration("trace-start")
            .with_start_key(Some(key.clone()))
            .with_trace(offer)
    };
    let created = registry
        .register_process_reporting_outcome(start(offer(FIRST, FIRST_ANCHOR)), &[])
        .await
        .expect("the first start");
    assert_eq!(created.outcome, crate::ProcessRegistrationOutcome::Created);
    let scope = DurableTraceScope {
        scope: TraceScopeId::admission(TraceScopeOwner::Process {
            process_id: created.record.id.clone(),
        }),
        cause: linked(FIRST),
        anchor: TraceAnchor::Context(context(FIRST_ANCHOR)),
        started_at_ms: created.record.created_at_ms,
    };
    assert_eq!(created.record.trace, Some(scope.clone()));

    // A host key fences the start's content, and the offer is not content:
    // a retry under another context and candidate is the retained process.
    for retry in [offer(SECOND, SECOND_ANCHOR), TraceScopeOffer::default()] {
        let retried = registry
            .register_process_reporting_outcome(start(retry), &[])
            .await
            .expect("a retry under another context is the same start");
        assert_eq!(retried.outcome, crate::ProcessRegistrationOutcome::Existing);
        assert_eq!(retried.record.id, created.record.id);
        assert_eq!(retried.record.trace, Some(scope.clone()));
        assert!(
            TraceScopeAdmission::of(scope.clone(), retried.is_created())
                .permit()
                .is_none(),
            "a retained start holds no emission permit"
        );
    }
    assert_eq!(
        registry
            .get_process(&created.record.id)
            .await
            .expect("read the process")
            .expect("the process is retained")
            .trace,
        Some(scope)
    );

    // Changed content under the key is the typed conflict it always was.
    let mut changed = start(offer(FIRST, FIRST_ANCHOR));
    changed.input = Arc::new(crate::ProcessInput::External {
        metadata: serde_json::json!({"changed": true}),
    });
    match registry
        .register_process_reporting_outcome(changed, &[])
        .await
    {
        Err(crate::PluginError::StartKeyConflict { start_key }) => assert_eq!(start_key, key),
        answer => panic!("changed content under a host key is refused typed: {answer:?}"),
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_redelivered_signal_keeps_its_first_cause(
    prefix: &str,
    stores: &Arc<dyn crate::StoreSet>,
) {
    let registry = stores.process_registry();
    let target = registry
        .register_process(
            crate::ProcessRegistration::new(
                crate::ProcessInput::Engine {
                    kind: "trace-signal".to_string(),
                    payload: serde_json::Value::Null,
                },
                crate::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            )
            .with_execution_env_ref(Some(crate::ProcessExecutionEnvRef::new(format!(
                "process-env:{prefix}-trace-signal"
            ))))
            .with_extra_event_types([super::process_registry::plain_event_type("signal.ready")]),
        )
        .await
        .expect("register the signal target");
    let signal = |payload: serde_json::Value, cause| {
        lash_core::ProcessSignal::new(
            lash_core::ProcessSignalIdentity::new(target.id.clone(), "ready", "one")
                .expect("a valid signal identity"),
            payload,
        )
        .with_trace_cause(cause)
    };
    let sent = signal(serde_json::json!(1), linked(FIRST));
    let resent = signal(serde_json::json!(1), linked(SECOND));
    assert!(
        sent.same_signal(&resent),
        "the cause is no part of the signal"
    );
    assert_eq!(sent.identity.append_key(), resent.identity.append_key());

    let first = registry
        .append_event(&target.id, sent.append_request())
        .await
        .expect("the first delivery");
    assert_eq!(first.realization, lash_core::StoreRealization::Realized);
    assert_eq!(first.event.semantics.trace_cause, linked(FIRST));
    let replay = registry
        .append_event(&target.id, resent.append_request())
        .await
        .expect("a redelivery under another context is the same signal");
    assert_eq!(replay.realization, lash_core::StoreRealization::Coalesced);
    assert_eq!(replay.event.sequence, first.event.sequence);
    assert_eq!(
        replay.event.semantics.trace_cause,
        linked(FIRST),
        "a redelivery reads the first cause back"
    );

    // A changed payload under the identity is the conflict it always was.
    let error = registry
        .append_event(
            &target.id,
            signal(serde_json::json!(2), linked(FIRST)).append_request(),
        )
        .await
        .expect_err("a changed signal under the same identity is refused");
    assert!(
        lash_core::is_durable_identity_conflict(&error),
        "the refusal is the durable-identity conflict: {error:?}"
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_redelivered_occurrence_keeps_its_first_scope(
    prefix: &str,
    stores: &Arc<dyn crate::StoreSet>,
) {
    let triggers = stores.trigger_store();
    let fire = |payload: serde_json::Value, offer: TraceScopeOffer| {
        crate::TriggerOccurrenceRequest::new(
            "ui.button.pressed",
            format!("{prefix}-trace-source"),
            payload,
            format!("{prefix}-trace-fire"),
        )
        .with_trace(offer)
    };
    let pressed = serde_json::json!({ "button": "Blue" });
    assert_eq!(
        lash_core::facade_support::deterministic_occurrence_id(&fire(
            pressed.clone(),
            offer(FIRST, FIRST_ANCHOR)
        )),
        lash_core::facade_support::deterministic_occurrence_id(&fire(
            pressed.clone(),
            TraceScopeOffer::default()
        )),
        "the offer is no part of the occurrence's identity"
    );

    let first = triggers
        .ingest_occurrence(fire(pressed.clone(), offer(FIRST, FIRST_ANCHOR)))
        .await
        .expect("the first ingest");
    assert_eq!(first.realization, lash_core::StoreRealization::Realized);
    let scope = DurableTraceScope {
        scope: TraceScopeId::admission(TraceScopeOwner::TriggerOccurrence {
            occurrence_id: first.occurrence.occurrence_id.clone(),
        }),
        cause: linked(FIRST),
        anchor: TraceAnchor::Context(context(FIRST_ANCHOR)),
        started_at_ms: first.occurrence.occurred_at_ms,
    };
    assert_eq!(first.occurrence.trace, Some(scope.clone()));

    for retry in [offer(SECOND, SECOND_ANCHOR), TraceScopeOffer::default()] {
        let again = triggers
            .ingest_occurrence(fire(pressed.clone(), retry))
            .await
            .expect("a redelivery under another context is the same fire");
        assert_eq!(again.realization, lash_core::StoreRealization::Coalesced);
        assert_eq!(again.occurrence, first.occurrence);
        assert!(
            TraceScopeAdmission::of(
                scope.clone(),
                again.realization == lash_core::StoreRealization::Realized,
            )
            .permit()
            .is_none(),
            "a redelivered fire holds no emission permit"
        );
    }

    // A changed fire under the idempotency key is the conflict it always was.
    let error = triggers
        .ingest_occurrence(fire(
            serde_json::json!({ "button": "Red" }),
            offer(FIRST, FIRST_ANCHOR),
        ))
        .await
        .expect_err("a changed fire under the same idempotency key is refused");
    assert!(
        lash_core::is_durable_identity_conflict(&error),
        "the refusal is the durable-identity conflict: {error:?}"
    );
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_resubmitted_intent_keeps_its_first_scope(
    parts: &ShiftParts,
    stores: &Arc<dyn crate::StoreSet>,
) {
    let registry = stores.process_registry();
    let owner = crate::RuntimeOwner::Session(parts.session_id.clone());
    let identity = crate::derive_tool_intent_identity(
        &owner,
        "trace-scope",
        &lash_core::ToolCallId::fixture("trace-call"),
        0,
    );
    // An emission carries a whole occurrence request: the offer beside its
    // payload is no part of the intent's payload hash.
    let emit = |fire: TraceScopeOffer| {
        crate::ToolIntent::EmitTrigger(lash_core::EmitTriggerIntent {
            owner: owner.clone(),
            request: crate::TriggerOccurrenceRequest::new(
                "ui.button.pressed",
                "trace-intent-source",
                serde_json::json!({ "button": "Blue" }),
                "trace-intent-fire",
            )
            .with_trace(fire),
        })
    };
    let submission = |fire: TraceScopeOffer, submitted: TraceScopeOffer, at_ms: u64| {
        crate::ToolIntentSubmissionRecord::new(identity.clone(), emit(fire))
            .expect("the submission has a payload hash")
            .with_trace_offer(submitted, at_ms)
    };
    let first = submission(TraceScopeOffer::default(), offer(FIRST, FIRST_ANCHOR), 7);
    let second = submission(
        offer(SECOND, SECOND_ANCHOR),
        offer(SECOND, SECOND_ANCHOR),
        9,
    );
    assert_eq!(
        first.payload_hash, second.payload_hash,
        "neither the submission's scope nor an offer inside the intent is hashed"
    );
    let scope = DurableTraceScope {
        scope: TraceScopeId::admission(TraceScopeOwner::ToolIntent {
            owner,
            replay_key: identity.replay_key.clone(),
        }),
        cause: linked(FIRST),
        anchor: TraceAnchor::Context(context(FIRST_ANCHOR)),
        started_at_ms: 7,
    };
    assert_eq!(first.trace, Some(scope.clone()));

    match registry
        .admit_tool_intent_submission(first)
        .await
        .expect("the first submission")
    {
        crate::ToolIntentSubmissionAdmission::Admitted => {}
        answer => panic!("the first submission claims the identity: {answer:?}"),
    }
    match registry
        .admit_tool_intent_submission(second.clone())
        .await
        .expect("a resubmission under another context")
    {
        crate::ToolIntentSubmissionAdmission::Existing(existing) => {
            assert_eq!(existing.payload_hash, second.payload_hash);
            assert_eq!(
                existing.trace,
                Some(scope),
                "a resubmission reads the first scope back"
            );
        }
        answer => panic!("a resubmission is answered the first writer: {answer:?}"),
    }
}
