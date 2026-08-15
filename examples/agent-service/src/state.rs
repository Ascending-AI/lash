use std::sync::{Arc, Mutex};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lash::sync::MutexExt;
use lash::{LashCore, LashSession, ModelSpec, TurnWorkDriver};
use serde_json::json;

use crate::db::AppDb;
use crate::demo_plugin::{DemoPlugin, DemoPluginConfig};

pub(crate) type AppResult<T> = Result<T, AppError>;

#[derive(Clone)]
pub(crate) struct AppStateData {
    core: LashCore,
    turn_work_driver: TurnWorkDriver,
    db: Arc<Mutex<AppDb>>,
    default_model: String,
    default_model_variant: Option<String>,
    #[cfg_attr(not(feature = "restate"), allow(dead_code))]
    durability: AgentServiceDurability,
    /// The dialect a newly created chat session is pinned to.
    ///
    /// Read once at construction rather than from the environment on every
    /// session open, so the value is injectable and a chat's dialect cannot
    /// change under it mid-process.
    rlm_dialect: lash::rlm::RlmDialect,
    #[cfg(feature = "restate")]
    restate_ingress_url: Option<String>,
    #[cfg(feature = "restate")]
    restate_http: reqwest::Client,
}

impl AppStateData {
    #[cfg(feature = "restate")]
    pub(crate) fn from_shared_db(
        core: LashCore,
        turn_work_driver: TurnWorkDriver,
        db: Arc<Mutex<AppDb>>,
        default_model: String,
        default_model_variant: Option<String>,
        durability: AgentServiceDurability,
        restate_ingress_url: Option<String>,
    ) -> Self {
        Self {
            core,
            turn_work_driver,
            db,
            default_model,
            default_model_variant,
            durability,
            restate_ingress_url,
            restate_http: reqwest::Client::new(),
        }
    }

    #[cfg(not(feature = "restate"))]
    pub(crate) fn new(
        core: LashCore,
        turn_work_driver: TurnWorkDriver,
        db: AppDb,
        default_model: String,
        default_model_variant: Option<String>,
        durability: AgentServiceDurability,
        rlm_dialect: lash::rlm::RlmDialect,
    ) -> Self {
        Self {
            core,
            turn_work_driver,
            db: Arc::new(Mutex::new(db)),
            default_model,
            default_model_variant,
            durability,
            rlm_dialect,
        }
    }

    /// The core, retained for the shutdown drain (trace flush).
    pub(crate) fn core(&self) -> &LashCore {
        &self.core
    }

    pub(crate) fn turn_work_driver(&self) -> &TurnWorkDriver {
        &self.turn_work_driver
    }

    pub(crate) fn default_model(&self) -> &str {
        &self.default_model
    }

    pub(crate) fn default_model_variant(&self) -> Option<&str> {
        self.default_model_variant.as_deref()
    }

    #[cfg(feature = "restate")]
    pub(crate) fn durability(&self) -> AgentServiceDurability {
        self.durability
    }

    #[cfg(feature = "restate")]
    pub(crate) fn restate_ingress_url(&self) -> Option<&str> {
        self.restate_ingress_url.as_deref()
    }

    #[cfg(feature = "restate")]
    pub(crate) fn restate_http(&self) -> &reqwest::Client {
        &self.restate_http
    }

    pub(crate) async fn open_session(
        &self,
        chat_id: &str,
        model: ModelSpec,
    ) -> AppResult<LashSession> {
        use lash::rlm::RlmSessionBuilderExt as _;

        let builder = || {
            self.core
                .session(chat_id)
                .session_spec(lash::SessionSpec::inherit().model(model.clone()))
                .plugin::<DemoPlugin>(DemoPluginConfig {
                    db: Arc::clone(&self.db),
                })
        };
        // The ambient dialect applies to a chat this call is creating. An
        // existing chat keeps the dialect recorded at its first commit: asking
        // for a different one is a hard error, and asserting it on every open
        // made every route fail against a store that predates the flip. Asking
        // and accepting the recorded answer is the same rule, stated so that a
        // reopen cannot break.
        if self.rlm_dialect == lash::rlm::RlmDialect::Lashlang {
            return Ok(builder().open().await?);
        }
        match builder()
            .rlm_dialect(lash::rlm::RlmDialect::Typescript)?
            .open()
            .await
        {
            Ok(session) => Ok(session),
            Err(error) if is_dialect_pin_conflict(&error) => Ok(builder().open().await?),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) async fn with_db<T, F>(&self, f: F) -> AppResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut AppDb) -> AppResult<T> + Send + 'static,
    {
        let db = Arc::clone(&self.db);
        tokio::task::spawn_blocking(move || {
            let mut db = db.lock_recover();
            f(&mut db)
        })
        .await
        .map_err(|err| AppError::internal(format!("database task failed: {err}")))?
    }

    pub(crate) async fn discard_pending_chat_fork(&self, chat_id: &str) -> AppResult<()> {
        let effect_host = self.core.effect_host();
        let execution_scope = self
            .core
            .session_delete_scope(chat_id)
            .await
            .map_err(|err| AppError::internal(err.to_string()))?;
        let scope = effect_host
            .scoped(execution_scope)
            .map_err(|err| AppError::internal(err.to_string()))?;
        self.core
            .delete_session(chat_id, scope)
            .await
            .map_err(|err| AppError::internal(err.to_string()))?;
        let chat_id = chat_id.to_string();
        self.with_db(move |db| db.delete_chat(&chat_id)).await
    }

    pub(crate) async fn recover_pending_chat_forks(&self) -> AppResult<()> {
        let pending = self.with_db(|db| db.pending_chat_forks()).await?;
        for chat_id in pending {
            self.discard_pending_chat_fork(&chat_id).await?;
        }
        Ok(())
    }
}

#[derive(Debug)]
pub(crate) struct AppError {
    pub(crate) status: StatusCode,
    pub(crate) message: String,
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AppError {}

impl AppError {
    pub(crate) fn bad_request(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.into(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.into(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<rusqlite::Error> for AppError {
    fn from(err: rusqlite::Error) -> Self {
        Self::internal(err.to_string())
    }
}

impl From<lash::EmbedError> for AppError {
    fn from(err: lash::EmbedError) -> Self {
        Self::internal(err.to_string())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentServiceDurability {
    Local,
    Restate,
}

impl AgentServiceDurability {
    pub(crate) fn configured() -> anyhow_like::Result<Self> {
        let mut args = std::env::args().skip(1);
        let mut from_args = None;
        while let Some(arg) = args.next() {
            if let Some(value) = arg.strip_prefix("--durability=") {
                from_args = Some(value.to_string());
                continue;
            }
            if arg == "--durability" {
                let value = args
                    .next()
                    .ok_or_else(|| "--durability requires local or restate".to_string())?;
                from_args = Some(value);
                continue;
            }
            return Err(format!("unknown argument `{arg}`"));
        }

        let raw = from_args
            .or_else(|| std::env::var("AGENT_SERVICE_DURABILITY").ok())
            .unwrap_or_else(|| "local".to_string());
        Self::parse(&raw)
    }

    fn parse(value: &str) -> anyhow_like::Result<Self> {
        match value {
            "local" => Ok(Self::Local),
            "restate" => Ok(Self::Restate),
            other => Err(format!(
                "invalid durability `{other}`; expected `local` or `restate`"
            )),
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Restate => "restate",
        }
    }
}

pub(crate) mod anyhow_like {
    pub(crate) type Result<T> = std::result::Result<T, String>;
}

/// Whether opening a session failed because it already recorded a different
/// dialect, as opposed to failing for any other reason.
///
/// Matched on the message because the pin lives in the protocol plugin and
/// surfaces as a protocol error; a narrower match would need the plugin's error
/// type in this example's dependency set. A wrong answer here can only make a
/// genuinely broken open retry once without the dialect and fail again.
fn is_dialect_pin_conflict(error: &lash::EmbedError) -> bool {
    error.to_string().contains("RLM dialect is durably pinned")
}

/// The dialect new chat sessions are created with, from `LASH_RUNBOOK_DIALECT`.
///
/// Read once at startup so the value is injected into the state rather than
/// consulted on every session open.
pub(crate) fn rlm_dialect_from_env() -> Result<lash::rlm::RlmDialect, String> {
    match std::env::var("LASH_RUNBOOK_DIALECT")
        .unwrap_or_else(|_| "lashlang".to_string())
        .as_str()
    {
        "lashlang" => Ok(lash::rlm::RlmDialect::Lashlang),
        "typescript" => Ok(lash::rlm::RlmDialect::Typescript),
        other => Err(format!(
            "LASH_RUNBOOK_DIALECT must be a registered RLM language id (`lashlang` or `typescript`), got `{other}`"
        )),
    }
}
