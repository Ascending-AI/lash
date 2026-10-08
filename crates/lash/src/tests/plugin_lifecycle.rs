//! A lifecycle observer is advisory (ADR 0132 §4): it sees each turn once
//! the turn's commit landed, it has no veto, and what it fails with changes
//! nothing that turn committed.

use super::*;
use crate::TurnInput;

/// A model answering its `n`th call `reply-n`, and every request's
/// messages as it saw them.
fn numbered_replies(requests: Arc<StdMutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("lifecycle-numbered")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            async move {
                let mut seen = requests.lock_recover();
                seen.push(format!("{:?}", request.messages));
                Ok(text_response(&format!("reply-{}", seen.len())))
            }
        })
        .build()
        .into_handle()
}

/// A failing `TurnFinalized` observer leaves its turn answered and
/// committed: each send answers, the observer saw each finalized turn, and a
/// new deployment over the same stores continues the transcript of both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observer_failure_is_advisory_and_keeps_committed_state() -> Result<()> {
    let finalized = Arc::new(StdMutex::new(Vec::<crate::TurnStatus>::new()));
    let requests = Arc::new(StdMutex::new(Vec::<String>::new()));
    let observer = {
        let finalized = Arc::clone(&finalized);
        Arc::new(StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("failing-observer"),
            lash_core::facade_support::PluginSpec::new().with_runtime_event(
                crate::hook_key!("turn-finalized"),
                Arc::new(move |event| {
                    let finalized = Arc::clone(&finalized);
                    Box::pin(async move {
                        if let lash_core::plugin::PluginLifecycleEvent::TurnFinalized(turn) = event
                        {
                            finalized
                                .lock_recover()
                                .push(crate::send::status_of_outcome(&turn.outcome));
                        }
                        Err(lash_core::PluginError::Session(
                            "observer sink unavailable".into(),
                        ))
                    })
                }),
            ),
        ))
    };
    let stores = sqlite_memory_store_set().await;
    let deploy = || {
        explicit_ephemeral_facets(LashCore::standard_builder(lash_conformance::backend_over(
            stores.clone(),
        )))
        .serve_test_llm_profile(
            numbered_replies(Arc::clone(&requests)),
            mock_llm_profile_spec(),
        )
        .plugin(observer.clone())
        .build(crate::testing::runtime_lease_owner())
    };
    let id = crate::SessionId::parse("failing-observer").expect("nonblank host identity");
    let core = deploy()?;
    let session = core
        .session(id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    for turn in 1..=2 {
        let output = session
            .send(TurnInput::text(format!("turn {turn}")))
            .output()
            .await?;
        assert_eq!(
            crate::send::status_of_outcome(&output.result.outcome),
            crate::TurnStatus::Answered,
            "turn {turn} answers despite its failing observer"
        );
    }
    assert_eq!(
        *finalized.lock_recover(),
        vec![crate::TurnStatus::Answered; 2],
        "the observer saw each finalized turn"
    );
    drop(session);
    core.shutdown().await?;

    let core = deploy()?;
    core.session(id)
        .open()
        .await?
        .send(TurnInput::text("turn 3"))
        .output()
        .await?;
    let requests = requests.lock_recover();
    let third = requests.last().expect("the third turn called the model");
    assert!(
        third.contains("reply-1") && third.contains("reply-2"),
        "both observed turns' replies are committed history: {third}"
    );
    drop(requests);
    core.shutdown().await?;
    Ok(())
}
