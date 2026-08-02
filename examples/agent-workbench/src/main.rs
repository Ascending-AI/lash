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
use std::sync::{Arc, Mutex};
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
use lash_provider_openai::{OPENROUTER_BASE_URL, OpenAiCompat, OpenAiCompatibleProvider};
use lash_remote_protocol::{
    RemoteLiveReplayGap, RemoteSessionObservation, RemoteSessionObservationEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;

const SESSION_ID_PREFIX: &str = "workbench";
const DEFAULT_CONTEXT_WINDOW_TOKENS: usize = 200_000;
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
include!("main_sections/routes.rs");
include!("main_sections/admin.rs");
include!("main_sections/app_state.rs");
include!("main_sections/plugins.rs");
include!("main_sections/prompt.rs");
include!("main_sections/tests.rs");
include!("main_sections/tests/process_work.rs");
include!("main_sections/tests/turn_control.rs");
include!("main_sections/tests/derived_notes.rs");
