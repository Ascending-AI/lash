use super::*;
use crate::TurnEvent;
use crate::support::{
    SessionObservationSubscription, SessionResume, TurnActivity, TurnInput, TurnOutcome,
};
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::ResponseTextMeta;
use lash_core::{SessionId, TurnId};

use tokio::sync::{Mutex as TokioMutex, oneshot};

mod builders_and_queue;
mod control_and_cancel;
mod observations;
mod publication_gap;
mod rlm_streaming;

/// How much stack an RLM law's thread gets: a cell's interpreter runs deep.
#[cfg(feature = "rlm")]
const STACK_BUDGET_BYTES: usize = 2 * 1024 * 1024;

/// Run `test` on its own current-thread runtime, on a thread with
/// [`STACK_BUDGET_BYTES`] of stack.
#[cfg(feature = "rlm")]
fn run_async_test_on_stack_budget<F, Fut, T>(name: &str, test: F) -> T
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = T> + 'static,
    T: Send + 'static,
{
    std::thread::Builder::new()
        .name(name.to_string())
        .stack_size(STACK_BUDGET_BYTES)
        .spawn(|| {
            let test = Box::pin(test());
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime")
                .block_on(test)
        })
        .expect("spawn stack-budget test thread")
        .join()
        .expect("stack-budget test thread")
}

/// The assistant prose a turn streamed, in order.
fn assistant_prose(events: &[TurnActivity]) -> String {
    events
        .iter()
        .filter_map(|activity| match &activity.event {
            lash_core::TurnEvent::AssistantProseDelta { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect()
}

/// `tools.app_lookup`, a tool whose attempt may defer.
fn app_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        )
        .expect("valid declared tool schemas"),
        "app_lookup",
    )
    .with_declaration(lash_core::ToolDeclaration::deferring())
}

/// `app_lookup` answering `{ "ok": true }`.
struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
    }
}

/// `app_lookup` failing every call.
#[cfg(feature = "rlm")]
struct FailingAppTools;

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for FailingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        lash_core::ToolOutcome::err_fmt("lookup failed but Lashlang recovered").into()
    }
}

/// `app_lookup` deferring its first call: it hands its completion key to
/// the law and stays pending until the host resolves it.
struct PendingAppTools {
    key_tx: StdMutex<Option<tokio::sync::oneshot::Sender<lash_core::PinnedKey>>>,
}

impl PendingAppTools {
    fn new(key_tx: tokio::sync::oneshot::Sender<lash_core::PinnedKey>) -> Self {
        Self {
            key_tx: StdMutex::new(Some(key_tx)),
        }
    }
}

#[async_trait]
impl ToolProvider for PendingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        assert_eq!(call.name(), "app_lookup");
        let key = match call.context.completion_key() {
            Ok(key) => key,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        if let Some(tx) = self.key_tx.lock_recover().take() {
            let _ = tx.send(key);
        }
        lash_core::ToolAttemptOutcome::Pending(lash_core::PendingCompletion::new())
    }
}

/// A provider that calls `app_lookup` once, then answers "done".
fn tool_roundtrip_provider() -> ProviderHandle {
    let responses = Arc::new(tokio::sync::Mutex::new(std::collections::VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "app_lookup".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        text_response("done"),
    ])));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn semantic_group_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(|_request| async move {
            Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Text {
                        text: "first".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:first".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                    LlmOutputPart::Text {
                        text: "second".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:second".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn retry_once_provider() -> ProviderHandle {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("retry-test")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |_request| {
            let attempts = Arc::clone(&attempts);
            async move {
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    return Err(LlmTransportError::new("retry me").with_retry_verdict(
                        lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                    ));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "retried".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn checkpoint_gated_provider(
    entered_tx: oneshot::Sender<()>,
    release_rx: oneshot::Receiver<()>,
) -> ProviderHandle {
    let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let release_rx = Arc::new(TokioMutex::new(Some(release_rx)));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("checkpoint-gated")
        .complete(move |request| {
            let entered_tx = Arc::clone(&entered_tx);
            let release_rx = Arc::clone(&release_rx);
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    if let Some(tx) = entered_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    Ok(text_response("first"))
                } else {
                    Ok(text_response(&format!(
                        "after {}",
                        last_user_text(&request)
                    )))
                }
            }
        })
        .build()
        .into_handle()
}

struct QueuedWorkHydrationProbeFactory {
    builds: Arc<AtomicUsize>,
}

impl lash_core::facade_support::PluginFactory for QueuedWorkHydrationProbeFactory {
    fn id(&self) -> &'static str {
        "queued-work-hydration-probe"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        self.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(QueuedWorkHydrationProbePlugin))
    }
}

impl lash_core::plugin::PluginDefinition for QueuedWorkHydrationProbeFactory {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("queued-work-hydration-probe")
    }
}

struct QueuedWorkHydrationProbePlugin;

impl lash_core::facade_support::SessionPlugin for QueuedWorkHydrationProbePlugin {
    fn id(&self) -> &'static str {
        "queued-work-hydration-probe"
    }

    fn register(
        &self,
        _reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}
