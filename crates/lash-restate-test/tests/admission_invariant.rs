//! A segment whose process breaks an admission invariant ends the process
//! Failed, typed, instead of failing only its invocation and stranding the
//! process Running with its awaiters parked (FIG-3819).
//!
//! The broken invariant forced here is a later segment's handover retained
//! without the execution start every later segment continues. The verdict step
//! meets it when the segment's start marker is already recorded; the start
//! step meets it when it is not. Either way the step journals the violation
//! as its value, and the segment stores one `process_segment_admission_invariant`
//! failure and publishes it to the process's awaiters.

#![expect(
    clippy::expect_used,
    reason = "test assertions; a failed unwrap is the test failure"
)]

use std::time::Duration;

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_core::{ProcessEventLogTestSupport as _, StoreSet as _};
use lash_restate_test::{RestateTestBackend, ServerConfig};

const CODE: &str = "process_segment_admission_invariant";

fn model_spec() -> lash_core::ModelSpec {
    lash_core::ModelSpec::builder("mock-model")
        .context_window_tokens(200_000)
        .build()
        .expect("model spec")
}

/// An RLM core over `restate`. The process runs no model; the provider only
/// has to exist.
fn build_core(restate: &RestateTestBackend) -> lash::LashCore {
    let backend = restate.lash_backend();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("admission-invariant")
        .complete(move |_request: LlmRequest| async move {
            Ok::<_, LlmTransportError>(LlmResponse::default())
        })
        .build()
        .into_handle();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(model_spec())
        .without_queued_work()
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "admission-invariant",
        ))
        .expect("build the lash core")
}

/// Where the broken invariant meets the admission.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Step {
    /// The segment's start marker is recorded: the verdict looks for the
    /// execution it continues.
    Verdict,
    /// No marker yet: the verdict admits and the start step looks for it.
    Start,
}

async fn admission_invariant_ends_the_process_failed(step: Step, seed: u64) {
    let restate = lash_restate_test::backend(seed, ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let core = build_core(&restate);
    restate.install_process_worker(
        lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .expect("the core's process worker configuration"),
        )
        .expect("build the process worker"),
    );
    let registration = lash_core::ProcessRegistration::new(
        lash_core::ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        lash_core::RecoveryContract::Rerunnable,
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    );
    let registry = restate.lash_backend().process_registry();
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register the process")
        .id;
    // Segment 1's handover is retained, but no execution start is: the
    // process never recorded the start every later segment continues.
    let continuations = restate.stores().process_continuations();
    continuations
        .put_segment_handover(
            &process_id,
            lash_core::PersistedSegmentHandover {
                segment_ordinal: 1,
                writer: String::new(),
                handover: lash_core::SegmentHandover {
                    reason: lash_core::BoundaryReason::JournalBudget,
                    program_hash: "program-v1".to_string(),
                    engine_state: vec![1],
                },
            },
        )
        .await
        .expect("retain segment 1's handover");
    if step == Step::Verdict {
        continuations
            .mark_segment_started(
                &lash_core::ProcessSegmentKey::new(process_id.clone(), 1),
                lash_core::SegmentStartMarker {
                    nonce: "an-earlier-execution".to_string(),
                    started_at_ms: 1,
                },
            )
            .await
            .expect("record segment 1's start marker");
    }

    let output = tokio::time::timeout(
        Duration::from_secs(20),
        restate
            .ingress()
            .call_workflow_json::<_, serde_json::Value>(
                "LashProcessWorkflow",
                &format!("{process_id}#1"),
                "run",
                &lash_restate::RestateProcessWorkflowInput {
                    process_id: process_id.clone(),
                    registration,
                    execution_context: lash_core::ProcessExecutionContext::default(),
                    segment_ordinal: 1,
                    journal_version: lash_restate::RESTATE_PROCESS_JOURNAL_VERSION,
                },
            ),
    )
    .await
    .expect("the segment's invocation ends")
    .expect("the segment returns the process's terminal, not an invocation failure");
    assert!(
        output.to_string().contains(CODE),
        "the segment publishes the typed failure: {output}"
    );

    let record = registry
        .get_process(&process_id)
        .await
        .expect("read the process")
        .expect("the process exists");
    assert_eq!(
        record.status,
        lash_core::ProcessStatus::Failed,
        "the process is not stranded Running: {record:?}"
    );
    let stored = record.outcome.clone().expect("a terminal is stored");
    assert!(
        serde_json::to_string(&stored)
            .expect("encode the stored terminal")
            .contains(CODE),
        "the stored failure is typed {CODE}: {stored:?}"
    );
    let events = registry
        .full_event_window(&process_id, 0)
        .await
        .expect("read the process events");
    let terminals = events
        .iter()
        .filter(|event| {
            matches!(
                event.event_type.as_str(),
                "process.completed" | "process.failed" | "process.cancelled" | "process.abandoned"
            )
        })
        .count();
    assert_eq!(terminals, 1, "exactly one terminal: {events:#?}");
    let awaited = tokio::time::timeout(
        Duration::from_secs(20),
        core.processes().await_output(&process_id),
    )
    .await
    .expect("await_output returns")
    .expect("await the process");
    assert_eq!(awaited, stored, "awaiters read the stored failure");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_verdict_admission_invariant_ends_the_process_failed() {
    admission_invariant_ends_the_process_failed(Step::Verdict, 0x3819).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_admission_invariant_ends_the_process_failed() {
    admission_invariant_ends_the_process_failed(Step::Start, 0x3819).await;
}
