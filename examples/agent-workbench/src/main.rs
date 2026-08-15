mod deferred_tools;
mod execution_graphs;
mod failure_provider;
mod mail;
mod restate;
mod restate_ingress;
mod ui;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::convert::Infallible;
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
use bytes::Bytes;
use chrono::Utc;
use futures_util::StreamExt;
use lash::observe::SessionCursor;
use lash::plugins::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
};
use lash::prompt::PromptContribution;
use lash::provider::{ProviderHandle, ProviderOptions};
use lash::triggers::TriggerEvent;
use lash::{
    LashCore, SessionSpec, TurnActivity, TurnActivitySink, TurnEvent, TurnResult,
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
    RemoteLiveReplayGap, RemoteSessionObservation, RemoteSessionObservationEvent,
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

include!("main_sections/bootstrap.rs");
include!("main_sections/stores.rs");
include!("main_sections/state.rs");
include!("main_sections/attachment_media.rs");
include!("main_sections/chat_projection.rs");
include!("main_sections/routes.rs");
include!("main_sections/session_routes.rs");
include!("main_sections/turn_ingress.rs");
include!("main_sections/admin.rs");
include!("main_sections/app_state.rs");
include!("main_sections/plugins.rs");
include!("main_sections/prompt.rs");
include!("main_sections/tests.rs");
include!("main_sections/tests/process_work.rs");
include!("main_sections/tests/turn_control.rs");
include!("main_sections/tests/derived_notes.rs");
