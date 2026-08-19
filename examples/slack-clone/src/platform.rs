//! The chat platform: a plain multiplayer product with a Slack-compatible API.
//!
//! **This half of the example deliberately has no Lash dependency.** It stands
//! in for "someone else's product" — the thing a bot is a guest inside. If a
//! change here needs to reach for `lash::…`, the example has lost its shape.

pub mod apps;
pub mod args;
pub mod db;
pub mod dispatch;
pub mod human_api;
pub mod state;
pub mod ui;
pub mod web_api;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context as _, Result};
use axum::Router;
use axum::routing::{get, post};

use crate::log_out;
use crate::store::SqliteHandle;
use state::PlatformState;

/// Platform configuration, all of it overridable by environment variable.
#[derive(Clone, Debug)]
pub struct PlatformConfig {
    /// Where the HTTP server listens.
    pub addr: SocketAddr,
    /// Root for the SQLite workspace file.
    pub data_dir: PathBuf,
    /// The static bot token the Web API accepts as `Authorization: Bearer …`.
    ///
    /// Real Slack mints this through OAuth per installation; see the README. The
    /// default deliberately does not start with `xoxb-`: a checked-in string
    /// shaped like a real bot token trips secret scanners and teaches the wrong
    /// reflex about where tokens may live.
    pub bot_token: String,
    /// The value stamped into every event envelope's deprecated `token` field.
    pub verification_token: String,
    /// Handle for the installed app's bot user.
    pub bot_handle: String,
    /// Workspace display name, returned by `auth.test`.
    pub team_name: String,
    /// Base delay for event-delivery retries. Doubles per attempt.
    ///
    /// Slack waits ~immediately, then 1 minute, then 5 minutes. An example that
    /// made you wait six minutes to watch a retry would not get watched, so the
    /// schedule is compressed. The retry *count* and headers are exact.
    pub retry_backoff: Duration,
    /// Per-attempt delivery timeout. Slack's is 3 seconds.
    pub delivery_timeout: Duration,
}

impl PlatformConfig {
    /// Read configuration from the environment, applying defaults.
    pub fn from_env() -> Result<Self> {
        let addr = std::env::var("SLACK_CLONE_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3040".to_string())
            .parse()
            .context("parse SLACK_CLONE_ADDR")?;
        Ok(Self {
            addr,
            data_dir: std::env::var("SLACK_CLONE_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(".slack-clone/platform")),
            bot_token: std::env::var("SLACK_CLONE_BOT_TOKEN")
                .unwrap_or_else(|_| "slack-clone-local-dev-token".to_string()),
            verification_token: std::env::var("SLACK_CLONE_VERIFICATION_TOKEN")
                .unwrap_or_else(|_| "slack-clone-dev-verification".to_string()),
            bot_handle: std::env::var("SLACK_CLONE_BOT_HANDLE")
                .unwrap_or_else(|_| "lashbot".to_string()),
            team_name: std::env::var("SLACK_CLONE_TEAM_NAME")
                .unwrap_or_else(|_| "Slack Clone".to_string()),
            retry_backoff: Duration::from_millis(
                std::env::var("SLACK_CLONE_RETRY_BACKOFF_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(1_000),
            ),
            delivery_timeout: Duration::from_millis(
                std::env::var("SLACK_CLONE_DELIVERY_TIMEOUT_MS")
                    .ok()
                    .and_then(|value| value.parse().ok())
                    .unwrap_or(3_000),
            ),
        })
    }
}

/// Build the platform's router over an already-seeded state.
///
/// The two namespaces are kept visibly apart. `/api/*` is the Slack-compatible
/// surface, named exactly after Slack's methods, and is all a bot ever touches.
/// `/platform/*` is this product's own UI surface and has no Slack equivalent —
/// so a reader can tell at a glance which routes are contract and which are
/// scaffolding.
pub fn router(state: PlatformState) -> Router {
    Router::new()
        .route("/", get(ui::index))
        .route("/healthz", get(human_api::healthz))
        // Slack-compatible Web API. Read methods accept GET or POST, as Slack's do.
        .route(
            "/api/auth.test",
            get(web_api::auth_test).post(web_api::auth_test),
        )
        .route("/api/chat.postMessage", post(web_api::chat_post_message))
        .route(
            "/api/conversations.list",
            get(web_api::conversations_list).post(web_api::conversations_list),
        )
        .route(
            "/api/conversations.history",
            get(web_api::conversations_history).post(web_api::conversations_history),
        )
        .route(
            "/api/conversations.replies",
            get(web_api::conversations_replies).post(web_api::conversations_replies),
        )
        .route(
            "/api/users.list",
            get(web_api::users_list).post(web_api::users_list),
        )
        // This product's own surface: identity, channel creation, live stream.
        .route("/platform/apps", post(human_api::register_app))
        .route("/platform/identify", post(human_api::identify))
        .route("/platform/bootstrap", get(human_api::bootstrap))
        .route("/platform/channels", post(human_api::create_channel))
        .route("/platform/messages", post(human_api::post_as_user))
        .route("/platform/history", get(human_api::history))
        .route("/platform/stream", get(human_api::stream))
        .with_state(state)
}

/// Boot the platform: open the store, seed the workspace, start the event
/// dispatcher, and serve until shutdown.
pub async fn run(config: PlatformConfig) -> Result<()> {
    let database = SqliteHandle::open(&config.data_dir.join("workspace.db"), db::SCHEMA)
        .context("open platform workspace store")?;
    let state = PlatformState::seed(config.clone(), database).await?;

    let dispatcher = dispatch::spawn(state.clone());

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("bind {}", config.addr))?;
    let identity = state.identity();
    log_out!(
        "slack-clone-platform listening on http://{} (team {}, app {}, bot user {})",
        config.addr,
        identity.team_id,
        identity.app_id,
        identity.bot_user_id
    );

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("serve platform")?;

    dispatcher.abort();
    Ok(())
}

/// Resolve on Ctrl-C or SIGTERM.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
    log_out!("slack-clone-platform draining");
}
