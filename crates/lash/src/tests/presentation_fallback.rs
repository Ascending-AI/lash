//! A tool call's presentation chain on the durable turn path: a step that
//! fails hands the next step the runtime's fallback presentation, and the
//! model is shown what the chain's last step made of it.

use super::*;
use crate::TurnInput;
use lash_core::ToolDefinitionBindingExt as _;

const SLOW: &str = "slow";

/// A tool that answers every call at once.
struct Answering;

fn definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{SLOW}"),
        SLOW,
        "Answers at once.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "object" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], SLOW))
}

#[async_trait]
impl ToolProvider for Answering {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == SLOW).then(|| Arc::new(definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "answered": true })).into()
    }
}

/// The previous return and the tool name one step was handed.
type Handed = (Vec<lash_core::facade_support::ModelToolReturnPart>, String);

/// What the presentation chain and the model saw, shared by every
/// deployment over one session.
#[derive(Clone, Default)]
struct Seen {
    /// Each step's runs, in order.
    calls: Arc<StdMutex<Vec<String>>>,
    /// The previous return and tool name the second step was handed.
    handed: Arc<StdMutex<Vec<Handed>>>,
    /// Every request the model was sent, rendered.
    requests: Arc<StdMutex<Vec<String>>>,
    /// Opens when the model is asked after the call.
    asked_after: Arc<tokio::sync::Notify>,
}

/// A plugin whose first presentation step fails and whose second replaces
/// the return it is handed.
fn chain(seen: &Seen) -> StaticPluginFactory {
    let failing: lash_core::plugin::ToolPresentationStep = {
        let calls = Arc::clone(&seen.calls);
        Arc::new(move |input| {
            calls.lock_recover().push("first".into());
            assert!(!input.previous.parts.is_empty());
            Box::pin(async {
                Err(lash_core::PluginError::Session(
                    "presentation fixture failed".into(),
                ))
            })
        })
    };
    let replacing: lash_core::plugin::ToolPresentationStep = {
        let (calls, handed) = (Arc::clone(&seen.calls), Arc::clone(&seen.handed));
        Arc::new(move |input| {
            calls.lock_recover().push("second".into());
            handed.lock_recover().push((
                input.previous.parts.clone(),
                input.previous.tool_name.clone(),
            ));
            Box::pin(async move {
                let mut replaced = input.previous;
                replaced.parts = vec![lash_core::facade_support::ModelToolReturnPart::text(
                    "replacement after fallback",
                )];
                Ok(replaced)
            })
        })
    };
    StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("fallback-chain"),
        lash_core::facade_support::PluginSpec::new()
            .with_presentation_step(crate::hook_key!("presentation-step-1"), failing)
            .with_presentation_step(crate::hook_key!("presentation-step-2"), replacing),
    )
}

/// A core over `backend` whose model calls [`SLOW`] first and answers
/// `done` after it, or, when `hold`, never answers after it.
fn deploy(backend: &lash_core::Backend, seen: &Seen, hold: bool) -> Result<LashCore> {
    let provider = {
        let (requests, asked_after) = (Arc::clone(&seen.requests), Arc::clone(&seen.asked_after));
        crate::testing::TestProvider::builder()
            .kind("presentation-fallback")
            .complete(move |request| {
                let rendered = format!("{:?}", request.messages);
                let first = !rendered.contains("call-presented");
                requests.lock_recover().push(rendered);
                if !first {
                    asked_after.notify_one();
                }
                async move {
                    if first {
                        return Ok(LlmResponse {
                            parts: vec![LlmOutputPart::ToolCall {
                                call_id: "call-presented".to_string(),
                                tool_name: SLOW.to_string(),
                                input_json: "{}".to_string(),
                                replay: None,
                            }],
                            ..LlmResponse::default()
                        });
                    }
                    if hold {
                        std::future::pending::<()>().await;
                    }
                    Ok(text_response("done"))
                }
            })
            .build()
            .into_handle()
    };
    explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(provider, mock_llm_profile_spec())
        .tools(Arc::new(Answering))
        .plugin(Arc::new(chain(seen)))
        .build(crate::testing::runtime_lease_owner())
}

/// A presentation step that fails is skipped with the runtime's fallback:
/// the next step is handed the failure's text as the previous return,
/// replaces it, and the model is shown the replacement. Each step runs
/// once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn presentation_step_failure_feeds_fallback_to_next_step() -> Result<()> {
    let seen = Seen::default();
    let core = deploy(&sqlite_memory_store_backend().await, &seen, false)?;
    let session = core
        .session(crate::SessionId::parse("presentation-fallback").expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let report = session.send(TurnInput::text("call it")).output().await?;
    assert!(report.is_success(), "{:?}", report.result.outcome);
    assert_eq!(
        *seen.handed.lock_recover(),
        vec![(
            vec![lash_core::facade_support::ModelToolReturnPart::text(
                "plugin session error: presentation fixture failed"
            )],
            SLOW.to_string()
        )],
        "the next step is handed the failed step's fallback"
    );
    let shown = seen
        .requests
        .lock_recover()
        .get(1)
        .cloned()
        .expect("the model was asked after the call");
    assert!(
        shown.contains("replacement after fallback")
            && !shown.contains("presentation fixture failed"),
        "the model is shown the chain's last presentation: {shown}"
    );
    assert_eq!(*seen.calls.lock_recover(), vec!["first", "second"]);
    drop(session);
    core.shutdown().await?;
    Ok(())
}

/// A presentation the turn recorded is served again, never derived again:
/// the owner is lost while the model's next call, which the presentation
/// rode in with, is in flight, and the next deployment over the same
/// stores re-sends that call with the recorded presentation, running
/// neither step again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_presentation_is_served_after_its_owners_loss_without_its_steps() -> Result<()> {
    const ID: &str = "presentation-redrive";
    let seen = Seen::default();
    let backend = sqlite_memory_store_backend().await;
    let core = deploy(&backend, &seen, true)?;
    let session_id = crate::SessionId::parse(ID).expect("nonblank host identity");
    let session = core
        .session(session_id.clone())
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let run = crate::TurnId::parse("presented-turn").expect("nonblank host identity");
    session
        .send(TurnInput::text("call it"))
        .id(run.clone())
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(30),
        seen.asked_after.notified(),
    )
    .await
    .expect("the model is asked after the call");
    drop(session);
    core.shutdown().await?;
    assert_eq!(*seen.calls.lock_recover(), vec!["first", "second"]);

    let core = deploy(&backend, &seen, false)?;
    let session = core.session(session_id).open().await?;
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        session.attach_id(run).output(),
    )
    .await
    .expect("the redriven turn answers")?;
    assert!(output.is_success(), "{:?}", output.result.outcome);
    assert_eq!(
        *seen.calls.lock_recover(),
        vec!["first", "second"],
        "the redrive serves the recorded presentation without its steps"
    );
    let shown = seen
        .requests
        .lock_recover()
        .last()
        .cloned()
        .expect("the redrive asked the model");
    assert!(
        shown.contains("replacement after fallback"),
        "the redrive shows the recorded presentation: {shown}"
    );
    drop(session);
    core.shutdown().await?;
    Ok(())
}
