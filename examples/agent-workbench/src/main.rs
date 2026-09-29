#[path = "../../shared/ndjson.rs"]
mod ndjson;
use ndjson::ndjson_response;
mod approvals;
mod deferred_tools;
mod execution_graphs;
mod failure_provider;
#[cfg(feature = "provider-wire-fixtures")]
#[path = "../../shared/local_restate.rs"]
mod local_restate;
mod mail;
#[path = "../../shared/prior_store_layout.rs"]
mod prior_store_layout;
mod restate;
mod restate_ingress;
#[path = "../../shared/shutdown_marker.rs"]
mod shutdown_marker;
mod ui;
#[cfg(feature = "provider-wire-fixtures")]
mod valid_empty_completion;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result as AnyhowResult, anyhow};
use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use chrono::Utc;
use futures_util::StreamExt;
use lash::observe::SessionCursor;
use lash::plugins::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};
use lash::prompt::PromptContribution;
use lash::provider::{ProviderHandle, ProviderOptions};
use lash::sync::MutexExt;
use lash::triggers::TriggerEvent;
use lash::{
    LashCore, SessionSpec, TurnActivity, TurnActivitySink, TurnEvent, TurnReport,
    tracing::{
        JsonlTraceSink, StderrTraceSink, TeeTraceSink, TraceContext, TraceEvent,
        TraceLashlangGraph, TraceLashlangGraphStore, TraceLevel, TraceRecord, TraceSink,
    },
};

#[cfg(test)]
fn test_core_owner() -> lash::persistence::LeaseOwnerIdentity {
    lash::persistence::LeaseOwnerIdentity::opaque(
        "agent-workbench-test-worker",
        "agent-workbench-test-boot",
    )
}
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};
use lash_remote_protocol::{
    Envelope, RemoteLiveReplayGap, RemoteSessionObservation, RemoteSessionObservationEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

const SESSION_ID_PREFIX: &str = "workbench";
/// The durable session roster, beside the current-selection `session-id` file.
const SESSION_ROSTER_FILE_NAME: &str = "sessions.json";
/// The longest session name the create form accepts.
const MAX_SESSION_NAME_CHARS: usize = 80;
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;
const AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS_ENV: &str = "AGENT_WORKBENCH_CONTEXT_WINDOW_TOKENS";
const AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS_ENV: &str = "AGENT_WORKBENCH_CONTINUE_AS_WARN_TOKENS";
/// The smallest context window the workbench accepts. The workbench is an
/// RLM host: its sessions switch frames through the model-driven
/// `continue_as` below this window, never through standard compaction.
const MIN_CONTEXT_WINDOW_TOKENS: usize = 40_000;
static WORKBENCH_CONTEXT_WINDOW_TOKENS: OnceLock<usize> = OnceLock::new();
const OPENROUTER_API_KEY_ENV: &str = "OPENROUTER_API_KEY";
pub(crate) const BUTTON_TRIGGER_RESOURCE: &str = "Button";
pub(crate) const BUTTON_TRIGGER_ALIAS: &str = "ui.button";
pub(crate) const BUTTON_TRIGGER_EVENT: &str = "pressed";
pub(crate) const BUTTON_TRIGGER_SOURCE_TYPE: &str = "ui.button.pressed";
pub(crate) const CRON_SCHEDULE_SOURCE_TYPE: &str = "cron.Schedule";
pub(crate) const MAIL_EVENT_RESOURCE: &str = "Mail";
pub(crate) const MAIL_EVENT_ALIAS: &str = "mail";
pub(crate) const MAIL_EVENT_EVENT: &str = "received";
pub(crate) const MAIL_RECEIVED_SOURCE_TYPE: &str = "mail.received";
const DEFAULT_TOKIO_THREAD_STACK_BYTES: usize = 8 * 1024 * 1024;

/// An attachment store of its own for a test state whose routes never read
/// the core's attachments.
#[cfg(test)]
fn test_attachment_store() -> Arc<dyn lash::persistence::AttachmentStore> {
    Arc::new(lash::persistence::FileAttachmentStore::new(
        std::env::temp_dir().join(format!(
            "agent-workbench-attachments-{}",
            uuid::Uuid::new_v4()
        )),
    ))
}
/// How long a cancel route stays attached waiting for a terminal before it
/// reports the cancellation as recorded-but-pending.
///
/// Under test the budget is deliberately large rather than small. No test
/// reaches it: every test that must observe the recorded-but-pending branch
/// drives it deterministically through the `TurnAttach` seam
/// (`with_test_attach`), so shortening this bound buys no suite time and only
/// makes the tests that assert the *attached* branch decide on how loaded the
/// machine is.
#[cfg(not(test))]
const TURN_TERMINAL_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const TURN_TERMINAL_ATTACH_TIMEOUT: Duration = Duration::from_secs(30);

#[path = "main_sections/bootstrap.rs"]
mod bootstrap;
pub(crate) use bootstrap::*;
#[path = "main_sections/stores.rs"]
mod stores;
pub(crate) use stores::*;
#[path = "main_sections/state.rs"]
mod state;
pub(crate) use state::*;
#[path = "main_sections/active_turns.rs"]
mod active_turns;
pub(crate) use active_turns::*;
#[path = "main_sections/turn_cancel.rs"]
mod turn_cancel;
pub(crate) use turn_cancel::*;
#[path = "main_sections/unknown_terminals.rs"]
mod unknown_terminals;
pub(crate) use unknown_terminals::*;
#[path = "main_sections/attachment_media.rs"]
mod attachment_media;
pub(crate) use attachment_media::*;
#[path = "main_sections/chat_projection.rs"]
mod chat_projection;
pub(crate) use chat_projection::*;
#[path = "main_sections/state_reads.rs"]
mod state_reads;
pub(crate) use state_reads::*;
#[path = "main_sections/routes.rs"]
mod routes;
pub(crate) use routes::*;
#[path = "main_sections/approval_routes.rs"]
mod approval_routes;
pub(crate) use approval_routes::*;
#[path = "main_sections/session_routes.rs"]
mod session_routes;
pub(crate) use session_routes::*;
#[path = "main_sections/turn_ingress.rs"]
mod turn_ingress;
pub(crate) use turn_ingress::*;
#[path = "main_sections/admin.rs"]
mod admin;
pub(crate) use admin::*;
#[path = "main_sections/session_open_retry.rs"]
mod session_open_retry;
pub(crate) use session_open_retry::*;
#[path = "main_sections/app_state.rs"]
mod app_state;
pub(crate) use app_state::*;
#[path = "main_sections/assistant_transcript.rs"]
mod assistant_transcript;
pub(crate) use assistant_transcript::*;
#[path = "main_sections/session_fence.rs"]
mod session_fence;
#[path = "main_sections/tool_loss_notice.rs"]
mod tool_loss_notice;
pub(crate) use session_fence::*;
#[path = "main_sections/plugins.rs"]
mod plugins;
pub(crate) use plugins::*;
#[path = "main_sections/prompt.rs"]
mod prompt;
pub(crate) use prompt::*;
#[cfg(test)]
#[path = "main_sections/tests/derived_notes.rs"]
mod derived_notes_tests;
#[cfg(test)]
#[path = "main_sections/tests/process_work.rs"]
mod process_work_tests;
#[cfg(test)]
#[path = "main_sections/tests.rs"]
mod tests;
#[cfg(test)]
#[path = "main_sections/tests/turn_control.rs"]
mod turn_control_timeout_tests;

#[cfg(test)]
/// This test crate's one path to a session that may not exist yet
/// (FIG-4112): only `create` creates, so this creates `session_id` with the
/// core's config unless the catalog already holds it, then hands back the
/// builder for the verb under test. An existing or deleted id is left for
/// that verb to report.
async fn created_session(
    core: &lash::LashCore,
    session_id: impl Into<lash::SessionId>,
) -> lash::SessionBuilder {
    let session_id = session_id.into();
    match core
        .session(session_id.clone())
        .create(lash::SessionCreation::default())
        .await
    {
        Ok(_)
        | Err(lash::EmbedError::SessionAlreadyExists { .. })
        | Err(lash::EmbedError::Store(lash::persistence::StoreError::SessionDeleted { .. })) => {}
        Err(error) => panic!("create session `{session_id}`: {error:?}"),
    }
    core.session(session_id)
}

fn main() -> AnyhowResult<()> {
    let stack_bytes = std::env::var("AGENT_WORKBENCH_TOKIO_STACK_BYTES")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(DEFAULT_TOKIO_THREAD_STACK_BYTES);
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(stack_bytes)
        .build()
        .context("build agent-workbench tokio runtime")?
        .block_on(async {
            let mut args = std::env::args().skip(1);
            if args.next().as_deref() == Some("register-deployment") {
                let endpoint_url = args
                    .next()
                    .context("usage: agent-workbench register-deployment <endpoint-url>")?;
                return register_deployment_command(&endpoint_url).await;
            }
            async_main().await
        })
}
