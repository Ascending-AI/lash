//! A plugin's direct completions outside a tool attempt, on the session
//! actor that owns them: here a compactor's, which a host's compaction runs.
//!
//! Unkeyed direct completions of one usage source are that source's
//! sequential program order, so one overlapping another of its lane is
//! refused; a model that requires streaming is handed a stream sender.

use super::*;

/// What a compactor's direct completions answered, as text or the refusal.
type Answers = Arc<StdMutex<Vec<(&'static str, std::result::Result<String, String>)>>>;

/// The direct completions a compactor makes.
#[derive(Clone)]
enum Script {
    /// The lane law's calls, the first held at the model on `gate`.
    Lanes(Arc<(tokio::sync::Notify, tokio::sync::Notify)>),
    /// One call.
    One,
}

/// A compactor that runs its script over its context's direct
/// completions, records what they answered and compacts nothing.
struct Calling {
    script: Script,
    answers: Answers,
}

#[async_trait]
impl lash_core::plugin::ContextCompactor for Calling {
    fn id(&self) -> &'static str {
        "direct-lanes"
    }

    async fn compact(
        &self,
        ctx: &lash_core::plugin::CompactionContext<'_>,
    ) -> std::result::Result<
        Option<lash_core::plugin::ContextCompaction>,
        lash_core::plugin::ContextError,
    > {
        let answers = run(&self.script, &ctx.direct_completions).await;
        self.answers.lock_recover().extend(answers);
        Ok(None)
    }
}

/// A direct completion's answer, as text or its refusal.
async fn ask(
    client: &lash_core::facade_support::DirectCompletionClient<'_>,
    request: lash_core::facade_support::DirectRequest,
    usage_source: &str,
) -> std::result::Result<String, String> {
    client
        .direct_completion(request, usage_source)
        .await
        .map(|completion| completion.text)
        .map_err(|error| error.to_string())
}

async fn run(
    script: &Script,
    client: &lash_core::facade_support::DirectCompletionClient<'_>,
) -> Vec<(&'static str, std::result::Result<String, String>)> {
    use lash_core::facade_support::DirectRequest;
    let Script::Lanes(gate) = script else {
        return vec![(
            "summarize",
            ask(client, DirectRequest::text("summarize"), "direct-test").await,
        )];
    };
    let (first, (other, overlap)) = tokio::join!(
        ask(client, DirectRequest::text("first"), "direct-test"),
        async {
            gate.0.notified().await;
            let other = ask(
                client,
                DirectRequest::text("other hook"),
                "other-plugin-hook",
            )
            .await;
            let overlap = ask(client, DirectRequest::text("overlap"), "direct-test").await;
            gate.1.notify_one();
            (other, overlap)
        }
    );
    let (a, b) = tokio::join!(
        ask(
            client,
            DirectRequest::text("a").with_replay_key("a"),
            "direct-test"
        ),
        ask(
            client,
            DirectRequest::text("b").with_replay_key("b"),
            "direct-test"
        ),
    );
    let after = ask(client, DirectRequest::text("after"), "direct-test").await;
    vec![
        ("first", first),
        ("other", other),
        ("overlap", overlap),
        ("a", a),
        ("b", b),
        ("after", after),
    ]
}

/// A session on a core serving `provider`, with the compactor running
/// `script`, compacted once by the host.
async fn compacted_with(provider: ProviderHandle, script: Script) -> Result<Answers> {
    let answers: Answers = Arc::default();
    let compactor = StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("direct-lanes"),
        lash_core::facade_support::PluginSpec::new().with_context_compactor(
            100,
            Arc::new(Calling {
                script,
                answers: Arc::clone(&answers),
            }),
        ),
    );
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .plugin(Arc::new(compactor))
    .build(crate::testing::runtime_lease_owner())?;
    let id = crate::SessionId::parse("direct-lanes").expect("nonblank host identity");
    core.session(id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let session = core.session(id).open().await?;
    let compacted = session.admin().state().compact_context(None).await?;
    assert!(!compacted, "the compactor compacts nothing");
    drop(session);
    core.shutdown().await?;
    Ok(answers)
}

/// A model whose call for `"first"` signals `entered` and holds until
/// `release`; every other call answers at once.
fn held_first(gate: Arc<(tokio::sync::Notify, tokio::sync::Notify)>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("direct-lanes-held")
        .complete(move |request| {
            let gate = Arc::clone(&gate);
            async move {
                if format!("{:?}", request.messages).contains("\"first\"") {
                    gate.0.notify_one();
                    gate.1.notified().await;
                }
                Ok(text_response("direct answer"))
            }
        })
        .build()
        .into_handle()
}

/// An unkeyed call overlapping another of its usage source is refused for
/// want of explicit replay keys; a distinct source owns a lane of its own;
/// keyed calls run concurrently; and the lane's guard is released once its
/// call returns.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_concurrency_requires_keys_and_releases_unkeyed_guard() -> Result<()> {
    let gate = Arc::new((tokio::sync::Notify::new(), tokio::sync::Notify::new()));
    let answers = compacted_with(held_first(Arc::clone(&gate)), Script::Lanes(gate)).await?;
    let answers = answers.lock_recover().clone();
    let answered = |name: &str| {
        answers
            .iter()
            .find(|(call, _)| *call == name)
            .map(|(_, answer)| answer.clone())
            .unwrap_or_else(|| panic!("no call {name}: {answers:?}"))
    };
    for name in ["first", "other", "a", "b", "after"] {
        assert_eq!(
            answered(name),
            Ok("direct answer".to_owned()),
            "{name}: {answers:?}"
        );
    }
    let overlap = answered("overlap").expect_err("the overlapping unkeyed call is refused");
    assert!(overlap.contains("explicit replay keys"), "{overlap}");
    Ok(())
}

/// A model that requires streaming is handed a stream sender for a direct
/// completion, which the request it was built from never carries.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn direct_effect_restores_required_streaming_for_provider_execution() -> Result<()> {
    let streamed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let provider = {
        let streamed = Arc::clone(&streamed);
        crate::testing::TestProvider::builder()
            .kind("direct-lanes-streaming")
            .requires_streaming(true)
            .complete(move |request| {
                let streamed = Arc::clone(&streamed);
                async move {
                    streamed.store(request.stream_events.is_some(), Ordering::SeqCst);
                    Ok(text_response("direct answer"))
                }
            })
            .build()
            .into_handle()
    };
    let answers = compacted_with(provider, Script::One).await?;
    assert_eq!(
        *answers.lock_recover(),
        vec![("summarize", Ok("direct answer".to_owned()))]
    );
    assert!(
        streamed.load(Ordering::SeqCst),
        "the model was handed a stream"
    );
    Ok(())
}
