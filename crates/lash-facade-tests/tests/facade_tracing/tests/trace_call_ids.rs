//! L26 (FIG-6299, figments' Laminar a4): a provider decorator's scope names
//! the exported `chat` span. Every attempt a provider is handed carries
//! the call's `llm_call_id`, the id the call's trace records and its
//! exported span carry, whether or not anything observes the runtime.

use super::trace_emission::{Observed, attribute};
use super::*;

use lash::provider::{LlmTransportError, TransportRetryVerdict};

const SESSION: &str = "l26-call-ids";
const PROMPT: &str = "direct prompt";
const DIRECT: &str = "direct_ask";
const DIRECT_OPERATION: &str = "tools.direct_ask";

fn direct_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{DIRECT}"),
        DIRECT_OPERATION,
        "Ask the model directly.",
        lash_vocabulary::tool_contract::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// A tool whose body completes [`PROMPT`] directly.
struct DirectAsk;

#[async_trait]
impl ToolProvider for DirectAsk {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash::plugins::PluginError,
    > {
        Ok(vec![direct_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == DIRECT_OPERATION).then(|| Arc::new(direct_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call
            .context
            .direct_completions()
            .complete(
                lash_core::facade_support::DirectRequest::text(PROMPT),
                "l26-direct",
            )
            .await
        {
            Ok(completion) => {
                lash_core::ToolOutcome::ok(serde_json::json!({ "text": completion.text })).into()
            }
            Err(error) => lash_core::ToolOutcome::err_fmt(error).into(),
        }
    }
}

/// What the decorator saw of one attempt: whether it was the tool body's
/// direct call, its scope's `request_id` and its `llm_call_id`.
type Seen = (
    bool,
    String,
    Option<String>,
    Option<lash::tracing::TraceCarrier>,
);

/// The decorator: a provider that records every attempt's scope. The
/// turn's first call fails its first attempt retryably and then asks for
/// [`DIRECT`]; its second answers.
fn decorated(seen: &Arc<StdMutex<Vec<Seen>>>) -> ProviderHandle {
    let seen = Arc::clone(seen);
    let turn_attempts = Arc::new(AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind("l26-decorator")
        .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(Some(2))
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |request: LlmRequest| {
            let direct = last_user_text(&request).contains(PROMPT);
            seen.lock_recover().push((
                direct,
                request.scope.request_id.clone(),
                request.scope.llm_call_id.clone(),
                request.scope.trace_carrier.as_deref().cloned(),
            ));
            let attempt = (!direct).then(|| turn_attempts.fetch_add(1, Ordering::SeqCst));
            async move {
                match attempt {
                    None => Ok(text_response("direct answer")),
                    Some(0) => Err(LlmTransportError::new("transient")
                        .with_retry_verdict(TransportRetryVerdict::RetryableTransient)),
                    Some(1) => Ok(LlmResponse {
                        parts: vec![LlmOutputPart::ToolCall {
                            call_id: "call-direct".to_owned(),
                            tool_name: DIRECT_OPERATION.replace('.', "__"),
                            input_json: "{}".to_owned(),
                            replay: None,
                        }],
                        ..LlmResponse::default()
                    }),
                    Some(_) => Ok(text_response("done")),
                }
            }
        })
        .build()
        .into_handle()
}

/// The scenario's attempts as the decorator saw them, on a core observed
/// by `observed` or by nothing.
async fn run(observe: bool) -> Result<(Vec<Seen>, Option<(Observed, LashCore)>)> {
    let seen = Arc::new(StdMutex::new(Vec::new()));
    let builder = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(decorated(&seen), mock_llm_profile_spec())
    .tools(lash::tool_provider_key!("tools"), Arc::new(DirectAsk));
    let (observed, builder) = if observe {
        let (observed, builder) = Observed::install(builder);
        (Some(observed), builder)
    } else {
        (None, builder)
    };
    let core = builder.build(lash_facade_tests::runtime_lease_owner())?;
    let session = core
        .session(
            lash_vocabulary::identity::SessionId::parse(SESSION).expect("nonblank host identity"),
        )
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let output = session
        .send(lash::TurnInput::text("ask directly"))
        .id(lash_vocabulary::identity::TurnId::parse("l26-turn").expect("nonblank host identity"))
        .output()
        .await?;
    assert!(output.is_success(), "{:?}", output.outcome);
    let seen = seen.lock_recover().clone();
    match observed {
        Some(observed) => Ok((seen, Some((observed, core)))),
        None => {
            core.shutdown().await?;
            Ok((seen, None))
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_attempt_names_the_exported_chat_span_of_its_call() -> Result<()> {
    let (seen, observed) = run(true).await?;
    let (observed, core) = observed.expect("the observed run");
    assert_eq!(
        seen.len(),
        4,
        "two attempts, a retry's turn call and a direct call: {seen:#?}"
    );
    let ids: Vec<String> = seen
        .iter()
        .map(|(_, _, id, _)| id.clone().expect("every attempt names its call"))
        .collect();
    assert_eq!(ids[0], ids[1], "a retried call's attempts carry one id");
    let called: std::collections::BTreeSet<_> = ids.iter().cloned().collect();
    assert_eq!(called.len(), 3, "three calls: {ids:?}");
    let exported: std::collections::BTreeSet<_> = observed
        .spans("chat")
        .iter()
        .map(|span| attribute(span, "lash.llm_call.id").expect("a chat span names its call"))
        .collect();
    assert_eq!(
        exported, called,
        "each call's exported span is the one its attempts name"
    );
    let closed: std::collections::BTreeSet<_> = ["llm_call_completed", "llm_call_failed"]
        .into_iter()
        .flat_map(|kind| observed.records(&core, kind))
        .map(|record| {
            record
                .context
                .llm_call_id
                .expect("a closing record names its call")
        })
        .collect();
    assert_eq!(closed, called, "each call's closing record names it");

    // FIG-6431: the decorator gets the actual logical chat span on every
    // attempt, including a direct model call made from a tool body.
    let started = observed.records(&core, "llm_call_started");
    for (_, _, id, carrier) in &seen {
        let opener = started
            .iter()
            .find(|record| &record.context.llm_call_id == id)
            .expect("each decorated call has an opener");
        assert_eq!(
            carrier.as_ref(),
            opener.position.as_ref().map(|position| &position.span)
        );
        assert!(carrier.is_some(), "an observed call has its own chat span");
    }

    // Unobserved, the decorator still sees the turn's calls named, by the
    // same ids, and the requests are unchanged.
    let (unobserved, _) = run(false).await?;
    let turn = |seen: &[Seen]| -> Vec<(String, Option<String>)> {
        seen.iter()
            .filter(|(direct, _, _, _)| !direct)
            .map(|(_, request, id, _)| (request.clone(), id.clone()))
            .collect()
    };
    assert_eq!(turn(&unobserved), turn(&seen));
    assert!(
        unobserved
            .iter()
            .all(|(direct, _, id, _)| !direct || id.is_some()),
        "an unobserved direct call is named too: {unobserved:#?}"
    );
    assert!(
        unobserved
            .iter()
            .all(|(_, _, _, carrier)| carrier.is_none()),
        "export off leaves the scope carrier explicitly absent"
    );
    core.shutdown().await?;
    Ok(())
}
