//! A diverged Restate process body parks the process (FIG-3674, R0b; the
//! Restate leg of the engine obligation L-E6).
//!
//! The laws execute the real `LashProcessWorkflow/run` handler through the
//! Restate protocol on the in-tree Endpoint double, with Restate's own retry:
//! each retry replays the journal the runtime acknowledged
//! ([`encode_journal_retry`]). A body that refuses to replay its journal
//! parks its process through the registry's park — non-terminal, no terminal
//! evidence — and fails the attempt retryably, so the invocation keeps its
//! journal. Every retry that refuses again re-parks the same park; the retry
//! a build that can replay the journal runs completes the process once and
//! closes the park.

use super::*;

/// Refuses to replay its journal on the first `diverging` runs, naming the
/// diverged effect kind, then completes.
struct DivergingRunner {
    runs: AtomicUsize,
    diverging: usize,
}

#[async_trait::async_trait]
impl RestateProcessRunner for DivergingRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        _scoped_effect_controller: lash_core::ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        if self.runs.fetch_add(1, Ordering::SeqCst) < self.diverging {
            return Err(PluginError::RuntimeEffectController(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::EffectReplayDivergence,
                    "recorded runtime effect did not match the reconstructed envelope",
                )
                .with_summary(lash_core::RuntimeEffectReplayMismatchReport {
                    divergent_path_count: 1,
                    first_divergent_paths: vec!["command.request.model".to_string()],
                    effect_kind: Some("llm_call".to_string()),
                }),
            ));
        }
        Ok(process_success(serde_json::json!({ "rerun": "completed" })).into())
    }
}

fn run_input(
    process_id: &ProcessId,
    registration: &ProcessRegistration,
) -> RestateProcessWorkflowInput {
    RestateProcessWorkflowInput {
        process_id: process_id.clone(),
        registration: registration.clone(),
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        sender_generation: crate::tests::test_build_generation(),
    }
}

async fn process_feed(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Vec<(lash_core::store::ParkId, lash_core::store::ParkEventKind)> {
    registry
        .process_park_feed(
            lash_core::store::ParkFeedCursor::initial(),
            std::num::NonZeroUsize::new(64).expect("non-zero"),
        )
        .await
        .expect("read the process park feed")
        .events
        .into_iter()
        .filter(|event| event.target == *process_id)
        .map(|event| (event.park_id, event.kind))
        .collect()
}

/// L-E6 on Restate: a diverged process body parks its process exactly once,
/// non-terminal and with no terminal evidence, re-parks the same park on
/// every retry that refuses again, and completes once — closing the park —
/// on the retry whose build can replay its journal.
#[tokio::test]
pub(super) async fn a_diverged_process_body_parks_once_and_completes_when_restored() {
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let registration = executed_registration();
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register the process")
        .id;
    let endpoint = Endpoint::builder()
        .bind(
            LashProcessWorkflowImpl::new_for_test(
                Arc::new(DivergingRunner {
                    runs: AtomicUsize::new(0),
                    diverging: 2,
                }),
                Arc::clone(&registry),
                Arc::clone(&stores.continuations),
            )
            .serve(),
        )
        .build();
    let input = run_input(&process_id, &registration);
    let diverged_reason = |record: &lash_core::ProcessRecord| {
        record
            .park()
            .map(|park| park.reason.clone())
            .expect("the diverged process is parked")
    };

    let first =
        invoke_process_workflow_endpoint(&endpoint, "run", process_id.as_str(), &input, true)
            .await
            .unwrap_or_default();
    assert!(
        restate_error_message(&first).is_some(),
        "a diverged attempt fails retryably, keeping its journal: {first:?}"
    );
    assert!(
        restate_output_failure_message(&first).is_none(),
        "a diverged attempt is never a terminal output: {first:?}"
    );
    let parked = registry
        .get_process(&process_id)
        .await
        .expect("read the diverged process")
        .expect("the diverged process is retained");
    assert!(!parked.is_terminal(), "a park is non-terminal: {parked:?}");
    assert_eq!(parked.outcome(), None, "a park writes no terminal evidence");
    assert_eq!(
        diverged_reason(&parked),
        lash_core::store::ParkReason::EffectReplayDivergence {
            effect_kind: "llm_call".to_string(),
            message: "recorded runtime effect did not match the reconstructed envelope".to_string(),
        },
        "the park names the diverged effect kind"
    );
    let opened = parked.park().cloned().expect("parked");
    assert_eq!(opened.attempts, 1);
    assert!(opened.refusing);

    // Restate retries the invocation over its acknowledged journal: the same
    // build refuses again, and the park is re-parked, not reopened.
    // Every retry replays the journal the first attempt acknowledged: a
    // refused attempt journals nothing past it.
    let journaled = restate_recorded_commands(&first).map_or(0, |commands| commands.len());
    let retry = || {
        encode_journal_retry(process_id.as_str(), &input, &first, journaled)
            .expect("encode Restate's retry")
    };
    let second = invoke_process_workflow_body(&endpoint, "run", retry(), true)
        .await
        .unwrap_or_default();
    assert!(
        restate_error_message(&second).is_some()
            && restate_output_failure_message(&second).is_none(),
        "a retry that refuses again fails retryably: {second:?}"
    );
    let reparked = registry
        .get_process(&process_id)
        .await
        .expect("read the re-parked process")
        .expect("the re-parked process is retained");
    let park = reparked.park().expect("the process stays parked");
    assert_eq!(park.park_id, opened.park_id, "a re-park keeps the park");
    assert_eq!(park.since_ms, opened.since_ms, "a re-park keeps its since");
    assert_eq!(park.attempts, 2, "a re-park counts the refusal");
    assert!(park.refusing);
    assert!(!reparked.is_terminal());
    assert_eq!(
        process_feed(&registry, &process_id).await,
        vec![(
            opened.park_id,
            lash_core::store::ParkEventKind::Parked {
                reason: opened.reason.clone()
            }
        )],
        "the feed records the park exactly once"
    );

    // The retry a build that can replay the journal runs publishes its
    // terminal through journaled runs first, then releases the journal pin
    // and seals the terminal's source before delivering it — each stage
    // journaled before the wait it suspends on.
    let publication = invoke_process_workflow_body(&endpoint, "run", retry(), false)
        .await
        .expect("the restored retry proposes its terminal publication");
    assert_eq!(
        restate_message_types(&publication),
        Some(vec![
            RESTATE_RUN_COMMAND_MESSAGE_TYPE,
            RESTATE_PROPOSE_RUN_COMPLETION_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]),
        "the restored retry proposes the terminal-publication run first"
    );
    let published = encode_journal_retry(
        process_id.as_str(),
        &input,
        &[&first[..], &publication[..]].concat(),
        journaled + 1,
    )
    .expect("encode Restate's retry over the published terminal");
    let second_publication = invoke_process_workflow_body(&endpoint, "run", published, false)
        .await
        .expect("the restored retry proposes its second terminal run");
    assert_eq!(
        restate_message_types(&second_publication),
        Some(vec![
            RESTATE_RUN_COMMAND_MESSAGE_TYPE,
            RESTATE_PROPOSE_RUN_COMPLETION_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]),
        "the published terminal lands before the scope's pin release"
    );
    let published_twice = encode_journal_retry(
        process_id.as_str(),
        &input,
        &[&first[..], &publication[..], &second_publication[..]].concat(),
        journaled + 2,
    )
    .expect("encode Restate's retry over both publication runs");
    let pin_release = invoke_process_workflow_body(&endpoint, "run", published_twice, false)
        .await
        .expect("the restored retry reaches its journal-pin release");
    let pin_release_calls = restate_call_frames(&pin_release)
        .map(|calls| {
            calls
                .iter()
                .map(|call| format!("{}/{}", call.service, call.handler))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    assert_eq!(
        pin_release_calls,
        vec!["LashDurableWaitIndex/release_process_journal"],
        "the published terminal precedes the scope's pin release"
    );
    assert_eq!(
        restate_message_types(&pin_release),
        Some(vec![
            RESTATE_CALL_COMMAND_MESSAGE_TYPE,
            RESTATE_SUSPENSION_MESSAGE_TYPE
        ]),
        "the restored retry journals the pin release"
    );
    let released = encode_journal_retry(
        process_id.as_str(),
        &input,
        &[
            &first[..],
            &publication[..],
            &second_publication[..],
            &pin_release[..],
        ]
        .concat(),
        journaled + 3,
    )
    .expect("encode Restate's retry over the released pin");
    let restored = invoke_endpoint_body_with_json_call_responses_then_suspend(
        &endpoint,
        "LashProcessWorkflow",
        "run",
        released,
        vec![serde_json::json!({ "status": "accepted" })],
    )
    .await
    .expect("the restored retry resolves its terminal's source and delivers it");
    assert!(
        restate_error_message(&restored).is_none(),
        "the restored retry completes: {restored:?}"
    );
    let completed = registry
        .get_process(&process_id)
        .await
        .expect("read the completed process")
        .expect("the completed process is retained");
    assert_eq!(completed.status(), lash_core::ProcessStatus::Completed);
    assert_eq!(completed.park(), None, "completion ends the park");
    assert_eq!(
        process_feed(&registry, &process_id).await,
        vec![
            (
                opened.park_id,
                lash_core::store::ParkEventKind::Parked {
                    reason: opened.reason.clone()
                }
            ),
            (
                opened.park_id,
                lash_core::store::ParkEventKind::Unparked {
                    cause: lash_core::store::UnparkCause::ProcessTerminal {
                        status: lash_core::ProcessStatus::Completed
                    }
                }
            ),
        ],
        "the park closes once, when the process completes"
    );
}
