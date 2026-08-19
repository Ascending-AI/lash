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
    /// The dialect the operator configured, if they configured one at all.
    ///
    /// `None` is "the operator said nothing", and it is a different instruction
    /// from naming Lashlang: an unconfigured service states nothing about a
    /// chat's dialect and runs whatever each chat recorded. Read once at
    /// construction rather than from the environment on every session open, so
    /// the value is injectable and a chat's dialect cannot change under it
    /// mid-process.
    rlm_dialect: Option<lash::rlm::RlmDialect>,
    #[cfg(feature = "restate")]
    restate_ingress_url: Option<String>,
    #[cfg(feature = "restate")]
    restate_http: reqwest::Client,
}

impl AppStateData {
    // Every parameter is a distinct required collaborator; the repo's
    // convention for constructors of this shape is the allow, not a config
    // struct (see `docs-snippets::persistence`, `workflow-graph-roundtrip`).
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "restate")]
    pub(crate) fn from_shared_db(
        core: LashCore,
        turn_work_driver: TurnWorkDriver,
        db: Arc<Mutex<AppDb>>,
        default_model: String,
        default_model_variant: Option<String>,
        durability: AgentServiceDurability,
        rlm_dialect: Option<lash::rlm::RlmDialect>,
        restate_ingress_url: Option<String>,
    ) -> Self {
        Self {
            core,
            turn_work_driver,
            db,
            default_model,
            default_model_variant,
            durability,
            rlm_dialect,
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
        rlm_dialect: Option<lash::rlm::RlmDialect>,
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
        let mut builder = self
            .core
            .session(chat_id)
            .session_spec(lash::SessionSpec::inherit().model(model))
            .plugin::<DemoPlugin>(DemoPluginConfig {
                db: Arc::clone(&self.db),
            });
        // A durable fact is stated, not requested (ADR 0066). The statement is
        // a guarded set-if-unset write: it lands on a chat that recorded
        // nothing, is a no-op on a chat that recorded the same dialect, and
        // refuses on one that recorded another. The refusal reaches the
        // operator; there is no reopen path that quietly runs the chat in its
        // old dialect. An unconfigured service states nothing at all.
        if let Some(configured) = self.rlm_dialect {
            builder = builder.plugin_option(
                lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash::rlm::RlmCreateExtras {
                    dialect: Some(configured),
                    ..lash::rlm::RlmCreateExtras::default()
                },
            )?;
        }
        Ok(builder.open().await?)
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

/// The dialect new chat sessions are created with, from `LASH_RUNBOOK_DIALECT`.
///
/// An unset variable is `None` — the operator stated nothing — rather than the
/// Lashlang default, so an unconfigured service never asserts a dialect against
/// a chat that recorded another one. Read once at startup so the value is
/// injected into the state rather than consulted on every session open.
pub(crate) fn rlm_dialect_from_env() -> Result<Option<lash::rlm::RlmDialect>, String> {
    let Ok(configured) = std::env::var("LASH_RUNBOOK_DIALECT") else {
        return Ok(None);
    };
    lash::rlm::RlmDialect::from_language_id(&configured)
        .map(Some)
        .ok_or_else(|| {
            format!(
                "LASH_RUNBOOK_DIALECT must be a registered RLM language id ({}), got `{configured}`",
                lash::rlm::RlmDialect::registered_language_ids()
            )
        })
}

/// The dialect a turn actually resolved, for prompt copy that has to be written
/// in one language.
///
/// ADR 0063: host copy follows the session's own dialect. This reads the same
/// resolved options the executor is handed, so a store that outlived a
/// configuration change is described in the dialect it is running rather than
/// the one this process was started with.
pub(crate) fn rlm_dialect_from_turn_options(
    options: &lash_core::ProtocolTurnOptions,
) -> lash::rlm::RlmDialect {
    options
        .decode::<lash_rlm_types::RlmCreateExtras>()
        .ok()
        .and_then(|extras| extras.dialect)
        .unwrap_or_default()
}
