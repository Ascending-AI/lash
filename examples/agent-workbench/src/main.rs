#[path = "../../shared/ndjson.rs"]
mod ndjson;
use ndjson::ndjson_response;
mod approvals;
mod deferred_tools;
mod execution_graphs;
mod failure_provider;
mod mail;
mod restate;
mod restate_ingress;
mod ui;

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
use lash_standard_plugins::{
    ROLLING_HISTORY_COMPACTION_BUFFER_TOKENS, rolling_history::RollingHistoryPluginFactory,
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
const MIN_CONTEXT_WINDOW_TOKENS: usize = ROLLING_HISTORY_COMPACTION_BUFFER_TOKENS * 2;
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

#[cfg(test)]
fn test_attachment_store() -> Arc<dyn lash::persistence::AttachmentStore> {
    Arc::new(lash::persistence::InMemoryAttachmentStore::new())
}
#[cfg(not(test))]
const TURN_TERMINAL_ATTACH_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(test)]
const TURN_TERMINAL_ATTACH_TIMEOUT: Duration = Duration::from_millis(250);

#[path = "main_sections/bootstrap.rs"]
mod bootstrap;
pub(crate) use bootstrap::*;
#[path = "main_sections/stores.rs"]
mod stores;
pub(crate) use stores::*;
#[path = "main_sections/state.rs"]
mod state;
pub(crate) use state::*;
#[path = "main_sections/turn_cancel.rs"]
mod turn_cancel;
pub(crate) use turn_cancel::*;
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
#[path = "main_sections/session_fence.rs"]
mod session_fence;
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
        .block_on(async_main())
}
