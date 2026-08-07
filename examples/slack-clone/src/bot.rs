//! The Lash bot: a guest inside the platform.
//!
//! This half is the reference embedding. It reaches the platform only through
//! [`slack_api::SlackApi`] — the same HTTP surface a real Slack app would use —
//! and it holds no privileged access to the platform's database. That constraint
//! is what makes the example honest about what "a bot inside someone else's
//! product" can and cannot do.

pub mod channel;
pub mod ledger;
pub mod runtime;
pub mod slack_api;
pub mod threads;
pub mod tools;
pub mod webhook;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};

use crate::store::SqliteHandle;
use channel::{BotIdentity, ChannelBot};
use ledger::EventLedger;
use runtime::RuntimeConfig;
use slack_api::SlackApi;

/// Bot configuration.
#[derive(Clone, Debug)]
pub struct BotConfig {
    /// Where the bot's HTTP server listens.
    pub addr: SocketAddr,
    /// Origin of the platform's Web API.
    pub api_base_url: String,
    /// URL the platform should POST events to. Defaults to `http://<addr>` plus
    /// [`webhook::EVENTS_PATH`].
    pub public_url: Option<String>,
    /// Bot token presented to the Web API.
    pub bot_token: String,
    /// Expected value of every event envelope's `token`.
    pub verification_token: String,
    /// Root for the bot's ledger and Lash stores.
    pub data_dir: PathBuf,
    /// JSONL trace destination.
    pub trace_path: Option<PathBuf>,
}

impl BotConfig {
    /// Read configuration from the environment, applying defaults.
    pub fn from_env() -> Result<Self> {
        let addr: SocketAddr = std::env::var("SLACK_CLONE_BOT_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3041".to_string())
            .parse()
            .context("parse SLACK_CLONE_BOT_ADDR")?;
        Ok(Self {
            addr,
            api_base_url: std::env::var("SLACK_CLONE_API_BASE_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:3040".to_string()),
            public_url: std::env::var("SLACK_CLONE_BOT_PUBLIC_URL").ok(),
            bot_token: std::env::var("SLACK_CLONE_BOT_TOKEN")
                .unwrap_or_else(|_| "slack-clone-local-dev-token".to_string()),
            verification_token: std::env::var("SLACK_CLONE_VERIFICATION_TOKEN")
                .unwrap_or_else(|_| "slack-clone-dev-verification".to_string()),
            data_dir: std::env::var("SLACK_CLONE_BOT_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from(".slack-clone/bot")),
            trace_path: std::env::var("SLACK_CLONE_BOT_TRACE")
                .ok()
                .map(PathBuf::from),
        })
    }

    /// The request URL the platform will deliver events to.
    pub fn request_url(&self) -> String {
        self.public_url
            .clone()
            .unwrap_or_else(|| format!("http://{}{}", self.addr, webhook::EVENTS_PATH))
    }
}

/// Boot the bot and serve until shutdown.
pub async fn run(config: BotConfig) -> Result<()> {
    let api = Arc::new(SlackApi::new(&config.api_base_url, &config.bot_token)?);
    // `auth.test` is the first call for a reason: the bot must learn its own
    // `U…` id before it can recognise a mention of itself. Retried because the
    // platform and the bot are independent processes with no start-up ordering.
    let identity = resolve_identity(&api).await?;
    println!(
        "slack-clone-bot is {} ({} / {}) in team {}",
        identity.handle, identity.bot_user_id, identity.bot_id, identity.team_id
    );

    let ledger_database = SqliteHandle::open(&config.data_dir.join("events.db"), ledger::SCHEMA)
        .context("open bot event ledger")?;
    let ledger = EventLedger::new(ledger_database);

    let mut runtime_config = RuntimeConfig::new(config.data_dir.join("lash"));
    runtime_config.trace_path = config.trace_path.clone();
    let (provider, model) = runtime::provider_from_env()?;
    let session_owner = runtime::session_owner(&runtime_config.incarnation);
    let core = runtime::build_core(&runtime_config, provider, model, Arc::clone(&api)).await?;

    let bot = Arc::new(ChannelBot::new(
        core,
        Arc::clone(&api),
        ledger,
        identity,
        config.verification_token.clone(),
        session_owner,
    ));
    if let Err(error) = bot.refresh_directory().await {
        eprintln!("slack-clone-bot could not preload the user directory: {error:#}");
    }

    let listener = tokio::net::TcpListener::bind(config.addr)
        .await
        .with_context(|| format!("bind {}", config.addr))?;
    let request_url = config.request_url();
    println!(
        "slack-clone-bot listening on http://{} (events at {request_url})",
        config.addr
    );

    let server = tokio::spawn({
        let bot = Arc::clone(&bot);
        async move {
            axum::serve(listener, webhook::router(bot))
                .with_graceful_shutdown(shutdown_signal())
                .await
        }
    });

    // Recover before registering, so a previous boot's unfinished work is settled
    // before the platform is asked to send more. Registration is the last step of
    // boot for exactly this reason.
    //
    // The pass cannot settle everything synchronously. A boot that restarts inside
    // the previous boot's session-execution lease TTL cannot take that lease, so an
    // interrupted turn's admission is still fenced and its event comes back
    // deferred. Those are retried on a background task rather than blocking boot
    // for the length of a lease TTL — the endpoint has live traffic to serve.
    match bot.recover().await {
        Ok(report) => {
            if !report.settled.is_empty() {
                println!(
                    "slack-clone-bot recovery settled {} event(s), deferred {}",
                    report.settled.len() - report.deferred.len(),
                    report.deferred.len()
                );
            }
            for event_id in report.deferred {
                let bot = Arc::clone(&bot);
                tokio::spawn(async move {
                    if let Err(error) = bot
                        .retry_deferred(event_id.clone(), channel::DEFERRED_RETRY_DEADLINE)
                        .await
                    {
                        eprintln!("slack-clone-bot deferred retry of {event_id} failed: {error:#}");
                    }
                });
            }
        }
        Err(error) => eprintln!("slack-clone-bot recovery pass failed: {error:#}"),
    }

    register(&api, &config, &request_url).await?;

    let served = server.await.context("bot server task failed")?;
    served.context("serve bot")?;
    if let Err(error) = bot.core().flush_trace_sink() {
        eprintln!("slack-clone-bot: trace flush failed: {error}");
    }
    Ok(())
}

/// Resolve the app's identity, waiting for the platform to come up.
async fn resolve_identity(api: &SlackApi) -> Result<BotIdentity> {
    let mut last_error = None;
    for attempt in 0..30 {
        match api.auth_test().await {
            Ok(auth) => {
                return Ok(BotIdentity {
                    bot_user_id: auth.user_id,
                    bot_id: auth.bot_id,
                    handle: auth.user,
                    team_id: auth.team_id,
                });
            }
            Err(error) => {
                if attempt == 0 {
                    println!(
                        "slack-clone-bot waiting for the platform at {}",
                        api.base_url()
                    );
                }
                last_error = Some(error);
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
    match last_error {
        Some(error) => Err(error).context("auth.test never succeeded"),
        None => bail!("auth.test never ran"),
    }
}

/// Register the Events API request URL with the platform.
///
/// On real Slack this is a click in the app-configuration UI, which triggers the
/// `url_verification` challenge. The platform exposes it as an endpoint so the
/// example can be started by a script; the handshake it performs is the same one.
async fn register(api: &SlackApi, config: &BotConfig, request_url: &str) -> Result<()> {
    let client = reqwest::Client::new();
    let url = format!("{}/platform/apps", api.base_url());
    let mut last_error = None;
    for _ in 0..20 {
        let response = client
            .post(&url)
            .bearer_auth(&config.bot_token)
            .json(&serde_json::json!({ "request_url": request_url }))
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => {
                println!("slack-clone-bot registered {request_url} with the platform");
                return Ok(());
            }
            Ok(response) => {
                let status = response.status();
                let body = response.text().await.unwrap_or_default();
                last_error = Some(anyhow::anyhow!("HTTP {status}: {body}"));
            }
            Err(error) => last_error = Some(error.into()),
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("registration never ran")))
        .context("register the Events API request url")
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
    println!("slack-clone-bot draining");
}
