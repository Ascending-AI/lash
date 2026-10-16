//! Finalization is an advisory notification identifying committed facts,
//! readable on any node without following the executing node's live stream.

use super::*;
use crate::TurnInput;

/// FIG-6546: an observer receives the committed identity and outcome kind.
/// Its failure cannot undo the commit, and another node reads the exact
/// turn's transcript and tool-call ledger after a later turn advanced the head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn finalization_identifies_a_committed_turn_readable_on_another_node() -> Result<()> {
    let (observed, mut observations) = tokio::sync::mpsc::unbounded_channel();
    let release_observer = Arc::new(tokio::sync::Semaphore::new(0));
    let observer_gate = Arc::clone(&release_observer);
    let observer = Arc::new(StaticPluginFactory::new(
        crate::plugins::PluginDeclaration::initial("finalized-identity"),
        crate::plugins::PluginSpec::new().with_runtime_event(
            crate::hook_key!("turn-finalized"),
            Arc::new(move |event| {
                let observed = observed.clone();
                let release = Arc::clone(&observer_gate);
                Box::pin(async move {
                    if let crate::plugins::PluginLifecycleEvent::TurnFinalized(turn) = event {
                        release
                            .acquire()
                            .await
                            .expect("the observer gate stays open")
                            .forget();
                        observed
                            .send(turn)
                            .expect("the law receives the notification");
                        return Err(crate::plugins::PluginError::Session(
                            "observer unavailable".into(),
                        ));
                    }
                    Ok(())
                })
            }),
        ),
    ));
    let prior = Arc::new(StdMutex::new(None::<crate::plugins::TurnFinalized>));
    let (plugin_read, mut plugin_reads) = tokio::sync::mpsc::unbounded_channel();
    let reader = Arc::new(StaticPluginFactory::new(
        crate::plugins::PluginDeclaration::initial("committed-reader"),
        crate::plugins::PluginSpec::new().with_before_turn(
            crate::hook_key!("read-prior-turn"),
            Arc::new({
                let prior = Arc::clone(&prior);
                move |ctx| {
                    let prior = prior.lock_recover().clone();
                    let plugin_read = plugin_read.clone();
                    Box::pin(async move {
                        if let Some(prior) = prior {
                            let read = ctx
                                .sessions
                                .committed_turn(&prior.session_id, &prior.turn_id)
                                .await
                                .expect("the plugin reads committed facts");
                            plugin_read
                                .send(read)
                                .expect("the law receives the plugin read");
                        }
                        Ok(crate::plugins::TurnContributions::default())
                    })
                }
            }),
        ),
    ));
    let sends = Arc::new(AtomicUsize::new(0));
    let model = crate::testing::TestProvider::builder()
        .kind("finalized-identity")
        .complete({
            let sends = Arc::clone(&sends);
            move |_| {
                let call = sends.fetch_add(1, Ordering::SeqCst);
                async move {
                    if call == 0 {
                        Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "lookup".into(),
                                tool_name: "app_lookup".into(),
                                input_json: "{}".into(),
                                replay: None,
                            }],
                            ..Default::default()
                        })
                    } else {
                        Ok(text_response(&format!("reply-{call}")))
                    }
                }
            }
        })
        .build()
        .into_handle();
    let stores = sqlite_memory_store_set().await;
    let deploy = |serves| {
        explicit_ephemeral_facets(LashCore::standard_builder(lash_conformance::backend_over(
            stores.clone(),
        )))
        .serve_sessions(serves)
        .serve_test_llm_profile(model.clone(), mock_llm_profile_spec())
        .tools(crate::tool_provider_key!("tools"), Arc::new(AppTools))
        .plugin(observer.clone())
        .plugin(reader.clone())
        .build(crate::testing::runtime_lease_owner())
    };
    let core = deploy(true)?;
    let session_id = crate::SessionId::from("finalized-identity");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    // The host awaits the outcome only; it does not follow its own send.
    let answer = session
        .send(TurnInput::text("look up app state"))
        .await?
        .outcome()
        .await?;
    assert_eq!(answer.status(), crate::TurnStatus::Answered);
    release_observer.add_permits(1);
    let first = observations
        .recv()
        .await
        .expect("the observer ran after commit");
    assert_eq!(&first.session_id, &session_id);
    assert_eq!(
        first.outcome,
        crate::persistence::TurnCommitOutcome::Completed
    );
    session
        .send(TurnInput::text("a later turn"))
        .await?
        .outcome()
        .await?;
    release_observer.add_permits(1);
    let later = observations.recv().await.expect("the later observer ran");
    assert_ne!(first.turn_id, later.turn_id);
    core.shutdown().await?;

    let other = deploy(true)?;
    let durable = other.session(session_id).durable().await?;
    let read = durable
        .committed_turn(&first.turn_id)
        .await?
        .expect("the turn committed");
    assert_eq!(&read.turn.turn_id, &first.turn_id);
    assert_eq!(read.turn.outcome, first.outcome);
    let texts = read
        .turn
        .entries
        .iter()
        .filter_map(|entry| match &entry.item {
            crate::transcript::TranscriptItem::Message(message) => Some(&message.blocks),
            _ => None,
        })
        .flatten()
        .filter_map(|block| match block {
            crate::transcript::TranscriptBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(texts.contains(&"reply-1"));
    assert!(!texts.contains(&"reply-2"));
    let [call] = read.tool_calls.as_slice() else {
        panic!("the committed turn holds one tool call")
    };
    assert_eq!(call.tool, "app_lookup");
    assert_eq!(call.args, serde_json::json!({}));
    assert!(call.output.is_success());
    assert_eq!(
        sends.load(Ordering::SeqCst),
        3,
        "the read invokes no provider"
    );
    assert!(
        durable
            .committed_turn(&crate::TurnId::fixture("unknown-turn"))
            .await?
            .is_none()
    );
    *prior.lock_recover() = Some(first);
    durable
        .send(TurnInput::text("read the first turn in a plugin"))
        .await?
        .outcome()
        .await?;
    release_observer.add_permits(1);
    observations
        .recv()
        .await
        .expect("the second node's observer ran");
    let plugin_read = plugin_reads
        .recv()
        .await
        .expect("the second node's plugin read the commit");
    assert_eq!(
        plugin_read,
        Some(read),
        "plugins and hosts read the same committed facts"
    );
    other.shutdown().await?;
    Ok(())
}
