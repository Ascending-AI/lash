//! L19 Q5 and L03 on the durable turn: cancelling a tool round's unfinished
//! member applies none of what it answered, however late it answers, on
//! the key its finished sibling wrote too.

use super::*;
use crate::{TurnCancelMode, TurnEvent, TurnInput};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::TurnCancelOutcome;

const STATE_PLUGIN: &str = "round-cancel-state";
const SET: &str = "state_set";

fn set_definition() -> lash_core::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_core::ToolDefinition::raw(
        format!("tool:{SET}"),
        SET,
        "Sets the plugin's key to the call's symbol.",
        object.clone(),
        object,
    )
    .expect("the state tool's schemas")
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], SET))
}

/// When the cancelled member `A` answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    /// Once its token fired: it observed the accepted cancel first.
    AfterItsStop,
    /// As soon as the host's cancel was accepted, whether or not its
    /// token's delivery reached it first.
    AfterAcceptance,
}

/// `A` opens `entered` and holds as its [`Answer`] says; `B` answers at
/// once. Each sets its key to its symbol.
#[derive(Clone)]
struct StatePlugin {
    answer: Answer,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
    /// Whether `A`'s body saw its token fired by the time it answered.
    observed: Arc<std::sync::atomic::AtomicBool>,
}

impl crate::plugins::PluginDefinition for StatePlugin {
    fn declaration() -> crate::plugins::PluginDeclaration {
        crate::plugins::PluginDeclaration::initial(STATE_PLUGIN)
    }
}

impl crate::plugins::PluginFactory for StatePlugin {
    fn id(&self) -> &'static str {
        STATE_PLUGIN
    }

    fn build(
        &self,
        _: &crate::plugins::PluginSessionContext,
    ) -> std::result::Result<Arc<dyn crate::plugins::SessionPlugin>, crate::plugins::PluginError>
    {
        Ok(Arc::new(self.clone()))
    }
}

impl crate::plugins::SessionPlugin for StatePlugin {
    fn id(&self) -> &'static str {
        STATE_PLUGIN
    }

    fn register(
        &self,
        reg: &mut crate::plugins::PluginRegistrar,
    ) -> std::result::Result<(), crate::plugins::PluginError> {
        reg.tools().provider(Arc::new(self.clone()))
    }
}

#[async_trait]
impl ToolProvider for StatePlugin {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![set_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == SET).then(|| Arc::new(set_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let symbol = call.args["symbol"].as_str().unwrap_or_default().to_owned();
        let key = call.args["key"].as_str().unwrap_or_default().to_owned();
        if symbol == "A" {
            self.entered.notify_one();
            let token = call
                .context
                .cancellation_token()
                .cloned()
                .expect("an attempt carries the cooperative token");
            match self.answer {
                Answer::AfterItsStop => token.cancelled().await,
                Answer::AfterAcceptance => {
                    let _release = self.release.acquire().await.expect("the gate stays open");
                }
            }
            self.observed.store(token.is_cancelled(), Ordering::SeqCst);
        }
        lash_core::ToolAttemptOutcome::Done {
            result: lash_core::ToolOutcomeDone::ok(serde_json::json!({ "set": symbol }))
                .with_state(crate::plugins::StateCommands::new().set(key, symbol.into())),
            intents: lash_core::ToolIntents::default(),
        }
    }
}

/// The session's committed plugin namespace of [`STATE_PLUGIN`].
async fn committed_values(
    core: &LashCore,
    id: &str,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>> {
    let store = lash_core::runtime::live_session_view(
        &core.store_factory,
        &crate::SessionId::fixture(id.to_string()),
    )
    .await?
    .expect("the session's store");
    let loaded = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("the committed head");
    Ok(loaded
        .state
        .plugin_state()
        .and_then(|state| state.plugins.get(STATE_PLUGIN))
        .map(|namespace| {
            namespace
                .values
                .iter()
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect()
        })
        .unwrap_or_default())
}

/// A round of `A` and `B` cancelled `Immediate` once `B`'s outcome is
/// published and while `A` holds; `A` answers as `answer` says. Both write
/// the key `value`. The committed
/// namespace, `A`'s completion if one was published, `B`'s, and whether `A`
/// saw its token fire.
struct CancelledRound {
    values: std::collections::BTreeMap<String, serde_json::Value>,
    a: Option<lash_core::ToolCallOutput>,
    b: lash_core::ToolCallOutput,
    observed: bool,
}

async fn cancelled_sibling(answer: Answer) -> Result<CancelledRound> {
    let id = format!("round-cancel-state-{answer:?}").to_ascii_lowercase();
    let (a_key, b_key) = ("value", "value");
    let plugin = StatePlugin {
        answer,
        entered: Arc::default(),
        release: Arc::new(tokio::sync::Semaphore::new(0)),
        observed: Arc::default(),
    };
    let provider = crate::testing::TestProvider::builder()
        .kind("round-cancel-state")
        .complete(move |_| async move {
            Ok(LlmResponse {
                parts: [("A", a_key), ("B", b_key)]
                    .into_iter()
                    .map(|(symbol, key)| LlmOutputPart::ToolCall {
                        call_id: format!("call-{symbol}"),
                        tool_name: SET.to_string(),
                        input_json: serde_json::json!({ "symbol": symbol, "key": key }).to_string(),
                        replay: None,
                    })
                    .collect(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle();
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(provider, mock_llm_profile_spec())
    .plugin(Arc::new(plugin.clone()))
    .build(crate::testing::runtime_lease_owner())?;
    let session = core
        .session(crate::SessionId::parse(&id).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    let handle = session.send(TurnInput::text("set both")).await?;
    let mut events = handle.events();
    plugin.entered.notified().await;
    // B's outcome is published, so durable, while A is unrecorded.
    let mut seen = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while let Some(activity) = events.next_activity().await {
            let activity = activity?;
            let b_done = matches!(&activity.event,
                TurnEvent::ToolCallCompleted { output, .. }
                    if output.value_for_projection()["set"] == "B");
            seen.push(activity);
            if b_done {
                return Result::Ok(());
            }
        }
        panic!("the run ended before B's outcome: {seen:?}")
    })
    .await
    .expect("B's outcome is published while A holds")?;
    let requested = match handle.cancel().mode(TurnCancelMode::Immediate).await? {
        crate::CancelReceipt::Cancelled { receipt, .. } => receipt.outcome,
        other => panic!("the cancel addressed the open run: {other:?}"),
    };
    assert!(
        matches!(requested, TurnCancelOutcome::Requested(_)),
        "{requested:?}"
    );
    plugin.release.add_permits(1);
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), handle.output())
        .await
        .expect("the cancelled turn ends")?;
    assert!(
        output.result.cancellation().is_some(),
        "{:?}",
        output.result.outcome
    );
    while let Some(activity) = events.next_activity().await {
        seen.push(activity?);
    }
    let completion = |symbol: &str| {
        seen.iter().find_map(|activity| match &activity.event {
            TurnEvent::ToolCallCompleted {
                provider_call_id,
                output,
                ..
            } if provider_call_id.as_deref() == Some(format!("call-{symbol}").as_str())
                || output.value_for_projection()["set"] == symbol =>
            {
                Some(output.clone())
            }
            _ => None,
        })
    };
    let a = completion("A");
    let b = completion("B").unwrap_or_else(|| panic!("B's completion is published: {seen:?}"));
    let values = committed_values(&core, &id).await?;
    drop(session);
    core.shutdown().await?;
    Ok(CancelledRound {
        values,
        a,
        b,
        observed: plugin.observed.load(Ordering::SeqCst),
    })
}

/// The cancelled member's answer is never applied: no committed value is
/// its own, and its completion, if published, is not a success.
async fn the_cancelled_member_applies_nothing(answer: Answer) -> Result<()> {
    let round = cancelled_sibling(answer).await?;
    assert!(round.b.is_success(), "{:?}", round.b);
    assert!(
        !round.values.values().any(|value| value == "A"),
        "the cancelled member's state was applied: {:?}",
        round.values
    );
    assert!(
        round.a.as_ref().is_none_or(|a| !a.is_success()),
        "the cancelled member's answer was recorded a success: {:?}",
        round.a
    );
    if answer == Answer::AfterItsStop {
        assert!(round.observed);
    }
    Ok(())
}

/// L19 Q5: the cancelled `A`, answering once it observed its
/// stop, applies nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn l19_native_inline_cancel_discards_the_same_key_unrecorded_sibling() -> Result<()> {
    the_cancelled_member_applies_nothing(Answer::AfterItsStop).await
}

/// L03: a cancel accepted before the unrecorded `A` answers discards its
/// answer, whether or not the token's delivery reached `A` first: the
/// accepted request, not the owner's live watch, decides it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "FIG-5381: a round member answering after an accepted cancel is recorded with its own outcome"]
async fn l03_native_cancel_accepted_before_inline_ack_discards_the_unrecorded_sibling() -> Result<()>
{
    the_cancelled_member_applies_nothing(Answer::AfterAcceptance).await
}
