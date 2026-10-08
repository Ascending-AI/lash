#[cfg(feature = "e2e-tools")]
#[path = "../../shared/e2e_commit_ledger.rs"]
mod e2e_commit_ledger;
#[cfg(feature = "e2e-tools")]
mod e2e_operation;
#[cfg(feature = "e2e-tools")]
mod e2e_receiver;
#[cfg(feature = "e2e-tools")]
mod e2e_receiver_hold;
#[cfg(feature = "e2e-tools")]
#[path = "../../shared/h2_fixture.rs"]
mod e2e_tools;
#[path = "../../shared/ndjson.rs"]
mod ndjson;
mod session_protocol;
use ndjson::ndjson_response;
mod approvals;
#[path = "../../shared/attachment_acceptance.rs"]
mod attachment_acceptance;
mod cron;
mod deferred_tools;
#[path = "../../shared/e2e_live_budget.rs"]
mod e2e_live_budget;
mod execution_graphs;
mod failure_provider;
mod mail;
mod mcp_fixture;
mod mcp_host;
mod mcp_policy;
#[path = "../../shared/shutdown_marker.rs"]
mod shutdown_marker;
mod turns;
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
use lash::provider::ProviderHandle;
use lash::sync::MutexExt;
use lash::triggers::TriggerEvent;
use lash::{
    LashCore, SessionSpec, TurnActivity, TurnActivitySink, TurnEvent,
    tracing::{
        JsonlTraceSink, StderrTraceSink, TeeTraceSink, TraceContext, TraceEvent,
        TraceLashlangGraph, TraceLashlangGraphStore, TraceLevel, TraceRecord, TraceSink,
    },
};

use lash::openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};
use lash::remote::Envelope;
use lash::remote::observations::RemoteLiveReplayGap;
use lash::remote::observations::RemoteSessionObservation;
use lash::remote::observations::RemoteSessionObservationEvent;
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
const AGENT_WORKBENCH_DELTA_FRAME_MS_ENV: &str = "AGENT_WORKBENCH_DELTA_FRAME_MS";
const AGENT_WORKBENCH_DELTA_FRAME_MAX_BYTES_ENV: &str = "AGENT_WORKBENCH_DELTA_FRAME_MAX_BYTES";
const AGENT_WORKBENCH_DELTA_FIRST_IMMEDIATE_ENV: &str = "AGENT_WORKBENCH_DELTA_FIRST_IMMEDIATE";
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

/// How long a cancel route stays attached waiting for a terminal before it
/// reports the cancellation as recorded-but-pending.
///
/// Under test the budget is deliberately large rather than small. Tests that
/// must observe the recorded-but-pending branch hold the attachment open
/// through the `TurnAttach` seam (`with_test_attach`) and hand the cancel
/// route a short deadline instead, so shortening this bound buys no suite
/// time and only makes the tests that assert the *attached* branch decide on
/// how loaded the machine is.
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
#[path = "main_sections/attachment_media.rs"]
mod attachment_media;
pub(crate) use attachment_media::*;
#[path = "main_sections/chat_rows.rs"]
mod chat_rows;
pub(crate) use chat_rows::*;
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
#[path = "main_sections/session_roster.rs"]
mod session_roster;
pub(crate) use session_roster::*;
#[path = "main_sections/file_replace.rs"]
mod file_replace;
pub(crate) use file_replace::*;
#[path = "main_sections/host_ingress.rs"]
mod host_ingress;
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
        .block_on(async {
            let mut args = std::env::args().skip(1);
            let command = args.next();
            if command.as_deref() == Some("mcp-fixture") {
                return mcp_fixture::serve().await;
            }
            async_main().await
        })
}
