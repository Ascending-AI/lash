//! FIG-4829: the first writer's trace provenance is retained.
//!
//! An admitted operation retains what caused it, and the span derived from
//! that cause and its identity, in the write that inserts its owning row. A retry of the same business submission under another
//! trace context is the same submission: it reads the retained provenance
//! back, changes no hash or key, and turns no retry into a conflict. A
//! changed submission is refused exactly as it was before any provenance
//! existed. An admission that finds its row already there reports an
//! existing scope, which holds no emission permit, so nothing is dispatched
//! or emitted under a span that lost.
//!
//! The law walks every owning row through the stores under test: a turn
//! input, a process start and a host-submitted tool intent.

use std::sync::Arc;

use lash_core::{
    DurableTraceScope, TraceCarrier, TraceCause, TraceLinks, TraceScopeAdmission, TraceScopeId,
    TraceScopeOffer, TraceScopeOwner,
};
use pretty_assertions::assert_eq;

/// The law's session and its store.
struct TraceParts {
    session_id: crate::SessionId,
    store: Arc<dyn crate::RuntimeStore>,
}

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

/// The cause of work producer `producer` parents.
fn caused(producer: u8) -> TraceCause {
    TraceCause::Parent(context(producer))
}

/// The offer of an admission caused by `producer`, carrying the span
/// `span`: a retained scope's span is derived from its cause and identity
/// whatever span an offer carried.
fn offer(producer: u8, span: u8) -> TraceScopeOffer {
    TraceScopeOffer::new(caused(producer), TraceLinks::new(), Some(context(span)))
}

/// The span the scope `scope` caused by `producer` retains.
fn derived(producer: u8, scope: &TraceScopeId) -> Option<TraceCarrier> {
    Some(lash_core::derive_span(
        &context(producer),
        &lash_core::SpanKey::Scope { scope },
    ))
}

const FIRST: u8 = 1;
const SECOND: u8 = 2;
const FIRST_SPAN: u8 = 0xa1;
const SECOND_SPAN: u8 = 0xa2;

/// Law P2 (C1): the first admission's cause and span are retained across
/// retried inputs, process starts and host tool intents, without changing a business hash, key
/// or typed conflict, and a retained read yields no emission permit.
pub async fn first_admission_wins_without_changing_business_identity(
    prefix: &str,
    stores: Arc<dyn crate::StoreSet>,
) {
    lash_execution_contracts::testing::process_execution_env_fixture(
        stores.process_env_store().as_ref(),
    )
    .await;
    let session_id = crate::SessionId::fixture(format!("{prefix}-trace-first-writer"));
    let parts = TraceParts {
        store: crate::conformance::law_session_store(stores.as_ref(), &session_id).await,
        session_id,
    };
    a_retried_input_keeps_its_first_cause(&parts).await;
    a_retried_start_keeps_its_first_scope(prefix, &stores).await;
    a_resubmitted_intent_keeps_its_first_scope(&parts, &stores).await;
}

fn input_draft(
    parts: &TraceParts,
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
    .with_trace_parent(cause.as_parent())
}

#[expect(
    clippy::expect_used,
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_retried_input_keeps_its_first_cause(parts: &TraceParts) {
    let draft = |cause| input_draft(parts, "traced-run", "the accepted words", cause);
    let digest = |draft: crate::PendingTurnInputDraft| {
        draft
            .submission_digest()
            .expect("the submission has a digest")
    };
    assert_eq!(
        digest(draft(caused(FIRST))),
        digest(draft(TraceCause::None)),
        "the cause is no part of the submission"
    );
    assert_eq!(digest(draft(caused(FIRST))), digest(draft(caused(SECOND))));

    let accepted = parts
        .store
        .enqueue_pending_turn_input(draft(caused(FIRST)))
        .await
        .expect("the first acceptance");
    assert_eq!(accepted.trace_cause, caused(FIRST));
    for retry in [caused(SECOND), TraceCause::None] {
        let retried = parts
            .store
            .enqueue_pending_turn_input(draft(retry))
            .await
            .expect("a retry under another context is the same submission");
        assert_eq!(retried.input_id, accepted.input_id);
        assert_eq!(
            retried.trace_cause,
            caused(FIRST),
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
        vec![caused(FIRST)]
    );

    // A changed submission is the typed conflict it always was.
    match parts
        .store
        .enqueue_pending_turn_input(input_draft(
            parts,
            "traced-run",
            "other words",
            caused(FIRST),
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
        .enqueue_pending_turn_input(untraced(TraceCause::None))
        .await
        .expect("the untraced acceptance");
    assert_eq!(accepted.trace_cause, TraceCause::None);
    let retried = parts
        .store
        .enqueue_pending_turn_input(untraced(caused(SECOND)))
        .await
        .expect("a traced retry of an untraced submission");
    assert_eq!(retried.input_id, accepted.input_id);
    assert_eq!(
        retried.trace_cause,
        TraceCause::None,
        "a retry cannot fill in a cause the first acceptance lacked"
    );
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
        .register_process_reporting_outcome(
            start(offer(FIRST, FIRST_SPAN))
                .with_trace_class(lash_execution_contracts::process::ProcessTraceClass::Workflow),
            &[],
        )
        .await
        .expect("the first start");
    assert_eq!(created.outcome, crate::ProcessRegistrationOutcome::Created);
    let id = TraceScopeId::admission(TraceScopeOwner::Process {
        process_id: created.record.id.clone(),
    });
    let scope = DurableTraceScope {
        span: derived(FIRST, &id),
        scope: id,
        cause: caused(FIRST),
        links: TraceLinks::new(),
        started_at_ms: created.record.created_at_ms,
    };
    assert_eq!(created.record.trace, Some(scope.clone()));
    assert_eq!(
        created.record.trace_class,
        lash_execution_contracts::process::ProcessTraceClass::Workflow
    );

    // A host key fences the start's content, and the offer is not content:
    // a retry under another context and span is the retained process.
    for retry in [offer(SECOND, SECOND_SPAN), TraceScopeOffer::default()] {
        let retried = registry
            .register_process_reporting_outcome(start(retry), &[])
            .await
            .expect("a retry under another context is the same start");
        assert_eq!(retried.outcome, crate::ProcessRegistrationOutcome::Existing);
        assert_eq!(retried.record.id, created.record.id);
        assert_eq!(retried.record.trace, Some(scope.clone()));
        assert_eq!(
            retried.record.trace_class,
            lash_execution_contracts::process::ProcessTraceClass::Workflow
        );
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
    let mut changed = start(offer(FIRST, FIRST_SPAN));
    changed.input = Arc::new(lash_execution_contracts::testing::held_engine_input(
        serde_json::json!({"changed": true}),
    ));
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
    clippy::panic,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_resubmitted_intent_keeps_its_first_scope(
    parts: &TraceParts,
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
    let cancel = || {
        crate::ToolIntent::CancelProcess(
            lash_execution_contracts::tool_intent::CancelProcessIntent {
                owner: owner.clone(),
                process_id: crate::ProcessId::fixture("trace-intent-target"),
            },
        )
    };
    let submission = |_fire: TraceScopeOffer, submitted: TraceScopeOffer, at_ms: u64| {
        crate::ToolIntentSubmissionRecord::new(identity.clone(), cancel())
            .expect("the submission has a payload hash")
            .with_trace_offer(submitted, at_ms)
    };
    let first = submission(TraceScopeOffer::default(), offer(FIRST, FIRST_SPAN), 7);
    let second = submission(offer(SECOND, SECOND_SPAN), offer(SECOND, SECOND_SPAN), 9);
    assert_eq!(
        first.payload_hash, second.payload_hash,
        "neither the submission's scope nor an offer inside the intent is hashed"
    );
    let id = TraceScopeId::admission(TraceScopeOwner::ToolIntent {
        owner,
        replay_key: identity.replay_key.clone(),
    });
    let scope = DurableTraceScope {
        span: derived(FIRST, &id),
        scope: id,
        cause: caused(FIRST),
        links: TraceLinks::new(),
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
