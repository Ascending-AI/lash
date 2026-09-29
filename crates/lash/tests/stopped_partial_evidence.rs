//! The host surface of a stopped turn's partial output (ADR 0114 §5): the
//! live and durable reports carry it, the session's read returns it, and a
//! host that resubmits it sends only ordinary input. Nothing truncated
//! reaches `AssistantOutput`, history or the next turn's context unless the
//! host sends it.
#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and these fixtures are test code too"
)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash::sync::MutexExt;
use lash::{
    CancelTarget, CaptureCoverage, CutState, InputItem, LashCore, LashSession, PartialItem,
    ReportSource, ResubmissionEligibility, ResubmissionSelection, StopReason, StoppedPartial,
    StoppedPartialRead, TurnId, TurnInput, TurnOutcome, TurnStop, build_resubmission,
};
use lash_core::llm::types::{
    LlmContentBlock, LlmOutputPart, LlmRequest, LlmResponse, LlmStreamEvent, StreamBlockIdentity,
};
use tokio::sync::oneshot;

const SEED: u64 = 0x0433_0005;
/// The prose the provider streams before the stop: mid-word, as a user sees
/// it when they press stop.
const STREAMED: &str = "The answer is fort";

/// A provider whose first call streams [`STREAMED`] and then hangs until the
/// turn is stopped; every later call records the user text it was sent and
/// answers `ok`.
struct Scripted {
    calls: Arc<AtomicUsize>,
    later_requests: Arc<Mutex<Vec<String>>>,
}

fn all_texts(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.to_string()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

async fn scripted_core(
    started: oneshot::Sender<()>,
) -> (LashCore, lash_restate_test::RestateTestBackend, Scripted) {
    let calls = Arc::new(AtomicUsize::new(0));
    let later_requests = Arc::new(Mutex::new(Vec::new()));
    let started = Arc::new(Mutex::new(Some(started)));
    let provider = lash_core::testing::TestProvider::builder()
        .requires_streaming(true)
        .complete({
            let calls = Arc::clone(&calls);
            let later_requests = Arc::clone(&later_requests);
            move |request| {
                let call = calls.fetch_add(1, Ordering::SeqCst);
                let started = started.lock_recover().take();
                let later_requests = Arc::clone(&later_requests);
                async move {
                    if call == 0 {
                        let stream = request.stream_events.expect("stream events");
                        let block = StreamBlockIdentity::new("text:0", 0);
                        stream.send(LlmStreamEvent::TextBlockStart {
                            block: block.clone(),
                        });
                        stream.send(LlmStreamEvent::Delta {
                            block,
                            text: STREAMED.to_string(),
                        });
                        if let Some(started) = started {
                            let _ = started.send(());
                        }
                        std::future::pending::<()>().await;
                        unreachable!("the stop drops the provider call");
                    }
                    later_requests.lock_recover().push(all_texts(&request));
                    Ok(LlmResponse {
                        parts: vec![LlmOutputPart::Text {
                            text: "ok".to_string(),
                            response_meta: None,
                        }],
                        response_metadata: Default::default(),
                        ..LlmResponse::default()
                    })
                }
            }
        })
        .build()
        .into_handle();
    let double = lash_restate_test::backend(SEED, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate double");
    let core = LashCore::standard_builder(double.lash_backend(), lash::TurnBudget::Unbounded)
        .provider(provider)
        .model(
            lash::ModelSpec::builder("mock-model")
                .context_window_tokens(16_000)
                .build()
                .expect("valid model spec"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "stopped-partial-evidence-worker",
            "stopped-partial-evidence-boot",
        ))
        .expect("core");
    (
        core,
        double,
        Scripted {
            calls,
            later_requests,
        },
    )
}

/// Send a turn, stop it once its prose streamed, and answer the stopped
/// report.
async fn stopped_turn(
    session: &LashSession,
    started: oneshot::Receiver<()>,
    root: &str,
) -> lash::TurnOutput {
    let handle = session
        .send(TurnInput::text("what is the answer?"))
        .id(root)
        .await
        .expect("send");
    let input_id = handle.input_id().clone();
    let settled = tokio::spawn(async move { handle.output().await });
    started.await.expect("the provider streamed");
    session
        .cancel(CancelTarget::Input(input_id))
        .origin("user")
        .await
        .expect("cancel");
    settled.await.expect("send task").expect("stopped report")
}

fn history_mentions(session: &LashSession, text: &str) -> bool {
    session
        .read_view()
        .messages()
        .iter()
        .any(|message| lash::message_text(message).contains(text))
}

fn assert_text_cut(partial: &StoppedPartial, root: &TurnId) {
    assert_eq!(partial.verify_digest(), Ok(()));
    assert_eq!(&partial.id.root, root);
    assert_eq!(&partial.id.turn_id, root);
    assert_eq!(partial.reason, StopReason::UserCancel);
    assert!(!partial.recovered_after_process_loss);
    assert_eq!(partial.coverage, CaptureCoverage::Complete);
    match partial.items.as_slice() {
        [PartialItem::Text { state, text, .. }] => {
            assert_eq!(*state, CutState::Interrupted);
            assert_eq!(text, STREAMED);
        }
        items => panic!("one interrupted text item, got {items:?}"),
    }
    assert_eq!(partial.eligibility(), ResubmissionEligibility::Ready);
    assert!(!partial.cut_mid_tool_call());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_turn_returns_its_partial_on_every_host_surface() {
    let (started_tx, started_rx) = oneshot::channel();
    let (core, _double, _scripted) = scripted_core(started_tx).await;
    let session = core
        .session("stopped-partial-surfaces")
        .open()
        .await
        .expect("open");
    let root = TurnId::from("stopped-root");

    let live = stopped_turn(&session, started_rx, root.as_str()).await;
    assert_eq!(live.result.source, ReportSource::Live);
    assert!(matches!(
        live.result.outcome,
        TurnOutcome::Stopped(TurnStop::Cancelled { .. })
    ));
    let partial = live
        .result
        .stopped_partial
        .clone()
        .expect("a stopped turn's report carries its partial");
    assert_text_cut(&partial, &root);
    // Returned beside the output, never inside it.
    assert!(!live.result.assistant_output.raw_text.contains(STREAMED));
    assert!(!live.result.assistant_output.safe_text.contains(STREAMED));
    assert!(!history_mentions(&session, STREAMED));

    // The session's read answers by root id and by physical turn id, on the
    // open session and on its durable handle alike.
    assert_eq!(
        session.stopped_partial(&root).await.expect("read"),
        StoppedPartialRead::Available(partial.clone())
    );
    assert_eq!(
        session
            .durable()
            .stopped_partial(&partial.id.turn_id)
            .await
            .expect("durable read"),
        StoppedPartialRead::Available(partial.clone())
    );
    assert_eq!(
        session
            .stopped_partial(&TurnId::from("never-sent"))
            .await
            .expect("read"),
        StoppedPartialRead::Unknown
    );

    // A handle attached after the live report was taken reads the durable
    // report, which carries the same partial.
    let durable = session
        .root(root.clone())
        .output()
        .await
        .expect("durable report");
    assert_eq!(durable.result.source, ReportSource::Durable);
    assert_eq!(durable.result.stopped_partial.as_ref(), Some(&partial));

    // The remote projection carries it too, and validates its digest.
    let remote = live.result.to_remote(
        &lash::SessionId::from("stopped-partial-surfaces"),
        &root,
        &live.activities,
    );
    assert_eq!(remote.stopped_partial.as_ref(), Some(&partial));
    remote.validate().expect("the remote report validates");
}

#[tokio::test(flavor = "multi_thread")]
async fn doing_nothing_backtracks_and_resubmitting_sends_ordinary_input() {
    let (started_tx, started_rx) = oneshot::channel();
    let (core, _double, scripted) = scripted_core(started_tx).await;
    let session = core
        .session("stopped-partial-flows")
        .open()
        .await
        .expect("open");
    let root = TurnId::from("stopped-flow-root");
    let stopped = stopped_turn(&session, started_rx, root.as_str()).await;
    let partial = stopped.result.stopped_partial.expect("partial");

    // Doing nothing: the next ordinary send's context holds none of the
    // stopped prose. That is the backtrack default.
    let finished_root = TurnId::from("finished-root");
    let next = session
        .send(TurnInput::text("never mind"))
        .id(finished_root.clone())
        .output()
        .await
        .expect("ordinary turn");
    assert!(matches!(next.result.outcome, TurnOutcome::Finished(_)));
    assert_eq!(next.result.stopped_partial, None);
    {
        let requests = scripted.later_requests.lock_recover();
        let context = requests.last().expect("the ordinary turn asked the model");
        assert!(!context.contains(STREAMED), "backtrack leaked: {context}");
    }
    assert!(!history_mentions(&session, STREAMED));
    assert_eq!(
        session.stopped_partial(&finished_root).await.expect("read"),
        StoppedPartialRead::NotStopped
    );

    // Continue in context: the helper renders ordinary text, and the host
    // sends it like any other input.
    let resubmission = build_resubmission(
        &ResubmissionSelection::defaults(&partial),
        TurnInput::text("please finish"),
    )
    .expect("a text cut resubmits by default");
    assert!(resubmission.omissions.omitted.is_empty());
    match resubmission.input.items.as_slice() {
        [
            InputItem::Text { text: excerpt },
            InputItem::Text { text: follow_up },
        ] => {
            assert!(excerpt.starts_with(lash::RESUBMISSION_PREAMBLE));
            assert!(excerpt.contains(STREAMED));
            assert_eq!(follow_up, "please finish");
        }
        items => panic!("an excerpt and the follow-up, got {items:?}"),
    }
    let continued = session
        .send(resubmission.input)
        .output()
        .await
        .expect("continued turn");
    assert!(matches!(continued.result.outcome, TurnOutcome::Finished(_)));
    {
        let requests = scripted.later_requests.lock_recover();
        let context = requests.last().expect("the continued turn asked the model");
        assert!(context.contains(STREAMED));
        assert!(context.contains("please finish"));
    }
    // The quoted prose is in history only as the user's input.
    let assistant_quotes = session.read_view().messages().iter().any(|message| {
        lash::message_role(message) == "assistant" && lash::message_text(message).contains(STREAMED)
    });
    assert!(
        !assistant_quotes,
        "no assistant message carries the stopped prose"
    );
    assert!(history_mentions(&session, STREAMED));
    assert!(
        !continued
            .result
            .assistant_output
            .raw_text
            .contains(STREAMED)
    );
    assert_eq!(scripted.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_read_answers_only_for_the_sessions_own_turns() {
    let (started_tx, _started_rx) = oneshot::channel();
    let (core, _double, scripted) = scripted_core(started_tx).await;
    // The scripted provider hangs on its first call; spend it on a turn this
    // test stops, so the finished turn below is answered.
    scripted.calls.store(1, Ordering::SeqCst);
    let session = core
        .session("stopped-partial-owner")
        .open()
        .await
        .expect("open");
    let finished = TurnId::from("finished-owner-root");
    let output = session
        .send(TurnInput::text("hello"))
        .id(finished.clone())
        .output()
        .await
        .expect("finished turn");
    assert!(matches!(output.result.outcome, TurnOutcome::Finished(_)));
    assert_eq!(output.result.stopped_partial, None);
    assert_eq!(
        session.stopped_partial(&finished).await.expect("read"),
        StoppedPartialRead::NotStopped
    );
    // Another session owns no such turn, and neither does this one.
    let stranger = core
        .session("stopped-partial-stranger")
        .open()
        .await
        .expect("open the other session");
    assert_eq!(
        stranger.stopped_partial(&finished).await.expect("read"),
        StoppedPartialRead::Unknown
    );
    assert_eq!(
        session
            .stopped_partial(&TurnId::from("never-sent"))
            .await
            .expect("read"),
        StoppedPartialRead::Unknown
    );
}
