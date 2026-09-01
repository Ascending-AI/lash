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
    /// The dialect this service runs chats in.
    ///
    /// There is no "said nothing" state: an unset `LASH_RUNBOOK_DIALECT` is the
    /// Lashlang default, and the service states it on every session open like
    /// any named id. A service that stated nothing would serve each chat in
    /// whatever it happened to record while the operator read one dialect off
    /// the environment — the mislabeled evidence the parity matrix exists to
    /// catch. Read once at construction rather than from the environment on
    /// every session open, so the value is injectable and a chat's dialect
    /// cannot change under it mid-process.
    rlm_dialect: lash::rlm::RlmDialect,
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
        rlm_dialect: lash::rlm::RlmDialect,
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
        // A durable fact is stated, not requested (ADR 0066). The statement is
        // a guarded set-if-unset write: it lands on a chat that recorded
        // nothing, is a no-op on a chat that recorded the same dialect, and
        // refuses on one that recorded another. The refusal reaches the
        // operator; there is no reopen path that quietly runs the chat in its
        // old dialect. Every open states a dialect, the default included.
        let builder = self
            .core
            .session(chat_id)
            .session_spec(lash::SessionSpec::inherit().model(model))
            .plugin::<DemoPlugin>(DemoPluginConfig {
                db: Arc::clone(&self.db),
            })
            .plugin_option(
                lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                lash::rlm::RlmCreateExtras {
                    dialect: Some(self.rlm_dialect),
                    ..lash::rlm::RlmCreateExtras::default()
                },
            )?;
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
/// An unset variable is the Lashlang default, stated on every session open like
/// any named id — the same answer every other shipped host gives, so one
/// environment produces one dialect everywhere. A chat that recorded another
/// dialect therefore fails its open loudly instead of being served in its
/// recorded dialect under a service that believes it is running Lashlang. Read
/// once at startup so the value is injected into the state rather than
/// consulted on every session open.
pub(crate) fn rlm_dialect_from_env() -> Result<lash::rlm::RlmDialect, String> {
    // The host's whole unset policy, in one line.
    Ok(lash::rlm::RlmDialect::from_env()?.unwrap_or_default())
}

/// The dialect the session recorded, for prompt copy that has to be written in
/// one language.
///
/// ADR 0063: host copy follows the session's own dialect. The dialect is
/// session scope (ADR 0066, FIG-1979) — a turn cannot restate it — so this
/// re-sources the prompt's language from the session config carried on the
/// options the hook is handed, and a store that outlived a configuration
/// change is described in the dialect it is running rather than the one this
/// process was started with.
///
/// The decode is strict. A malformed or unknown language id is a refusal, not
/// the default: silently substituting Lashlang is the very substitution
/// `RlmDialect::from_language_id` refuses by design, and it would word the
/// board prompt in one dialect while the cells executed the other.
pub(crate) fn rlm_session_dialect(
    options: &lash::runtime::ProtocolTurnOptions,
) -> Result<lash::rlm::RlmDialect, lash::plugins::PluginError> {
    lash::rlm::rlm_session_dialect(options)
        .map_err(|err| lash::plugins::PluginError::Session(err.to_string()))
}

#[cfg(test)]
mod dialect_pin_tests {
    use super::*;

    fn mock_model_spec() -> ModelSpec {
        ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("model spec")
    }

    async fn test_core(data_dir: &std::path::Path) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-dialect-pin")
            .build()
            .into_handle();
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            Arc::new(
                lash_sqlite_store::Store::open(&data_dir.join("artifacts.db"))
                    .await
                    .expect("artifact store"),
            ),
        );
        LashCore::rlm_builder(lash::TurnBudget::Unbounded, factory)
            .with_native_queued_work()
            .provider(provider)
            .model(mock_model_spec())
            .store_factory(Arc::new(lash_sqlite_store::SqliteSessionStoreFactory::new(
                data_dir.join("lash-sessions"),
            )))
            .effect_host(Arc::new(
                lash::durability::NativeEffectHost::default()
                    .allow_process_lifetime_completion_keys(),
            ))
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .process_env_store(Arc::new(
                lash_sqlite_store::Store::open(&data_dir.join("process-env.db"))
                    .await
                    .expect("process env store"),
            ))
            .trigger_store(Arc::new(
                lash_sqlite_store::SqliteTriggerStore::open(&data_dir.join("triggers.db"))
                    .await
                    .expect("trigger store"),
            ))
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                data_dir.join("attachments"),
            )))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-dialect-pin",
                "test",
            ))
            .expect("core")
    }

    fn state_with_dialect(
        core: &LashCore,
        db: AppDb,
        rlm_dialect: lash::rlm::RlmDialect,
    ) -> AppStateData {
        #[cfg(feature = "restate")]
        {
            AppStateData::from_shared_db(
                core.clone(),
                core.turn_work_driver(),
                Arc::new(Mutex::new(db)),
                "mock-model".to_string(),
                None,
                AgentServiceDurability::Local,
                rlm_dialect,
                None,
            )
        }
        #[cfg(not(feature = "restate"))]
        {
            AppStateData::new(
                core.clone(),
                core.turn_work_driver(),
                db,
                "mock-model".to_string(),
                None,
                AgentServiceDurability::Local,
                rlm_dialect,
            )
        }
    }

    /// A chat that recorded one dialect refuses to reopen under another, and
    /// the refusal reaches the caller instead of a quiet fallback.
    ///
    /// The second service here is the *unconfigured* one: `LASH_RUNBOOK_DIALECT`
    /// unset now means the Lashlang default, stated on every open like a named
    /// id. So a store carried over from a TypeScript run fails the open loudly
    /// rather than being served in its recorded dialect under a service that
    /// believes it is running Lashlang.
    #[tokio::test]
    async fn a_recorded_chat_refuses_to_reopen_under_another_dialect() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let core = test_core(data_dir).await;

        let typescript = state_with_dialect(
            &core,
            AppDb::open(&data_dir.join("app-typescript.db")).expect("app db"),
            lash::rlm::RlmDialect::Typescript,
        );
        let session = typescript
            .open_session("carried-over-chat", mock_model_spec())
            .await
            .expect("the first open pins the chat to TypeScript");
        session.close().await.expect("close the pinned session");

        let unconfigured = state_with_dialect(
            &core,
            AppDb::open(&data_dir.join("app-default.db")).expect("app db"),
            lash::rlm::RlmDialect::default(),
        );
        let Err(error) = unconfigured
            .open_session("carried-over-chat", mock_model_spec())
            .await
        else {
            panic!("a recorded dialect cannot be reopened as another one");
        };
        assert!(
            error.message.contains(
                "RLM session dialect is durably pinned to `typescript` and cannot be set to `lashlang`"
            ),
            "the refusal must name both dialects: {error}"
        );
    }

    /// A chat the service opens under its own dialect keeps opening.
    ///
    /// The guarded write is a no-op on agreement, so the default-stating
    /// service reopens the chats it created without a second thought — the
    /// refusal above is a disagreement, not a reopen tax.
    #[tokio::test]
    async fn a_chat_reopens_under_the_dialect_it_recorded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let core = test_core(data_dir).await;

        let service = state_with_dialect(
            &core,
            AppDb::open(&data_dir.join("app.db")).expect("app db"),
            lash::rlm::RlmDialect::default(),
        );
        let session = service
            .open_session("own-chat", mock_model_spec())
            .await
            .expect("first open");
        session.close().await.expect("close");

        let reopened = service
            .open_session("own-chat", mock_model_spec())
            .await
            .expect("a chat reopens under the dialect it recorded");
        reopened.close().await.expect("close the reopened session");
    }
}
