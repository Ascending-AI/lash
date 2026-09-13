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
    #[cfg(feature = "restate")]
    restate_ingress_url: Option<String>,
    #[cfg(feature = "restate")]
    restate_authority_id: Option<lash_restate::RestateAuthorityId>,
    #[cfg(feature = "restate")]
    restate_http: reqwest::Client,
}

impl AppStateData {
    // Every parameter is a distinct required collaborator; the repo's
    // convention for constructors of this shape is the allow, not a config
    // struct (see `workflow-graph-roundtrip`).
    #[allow(clippy::too_many_arguments)]
    #[cfg(feature = "restate")]
    pub(crate) fn from_shared_db(
        core: LashCore,
        turn_work_driver: TurnWorkDriver,
        db: Arc<Mutex<AppDb>>,
        default_model: String,
        default_model_variant: Option<String>,
        durability: AgentServiceDurability,
        restate_ingress_url: Option<String>,
        restate_authority_id: Option<lash_restate::RestateAuthorityId>,
    ) -> Self {
        Self {
            core,
            turn_work_driver,
            db,
            default_model,
            default_model_variant,
            durability,
            restate_ingress_url,
            restate_authority_id,
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
    ) -> Self {
        Self {
            core,
            turn_work_driver,
            db: Arc::new(Mutex::new(db)),
            default_model,
            default_model_variant,
            durability,
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
    pub(crate) fn restate_authority_id(&self) -> Option<&lash_restate::RestateAuthorityId> {
        self.restate_authority_id.as_ref()
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
        // TypeScript is the sole RLM language (ADR 0096), so a chat states no
        // language at its open: there is nothing left to pin, and a bag that
        // still records the retired `dialect` field is refused by the protocol
        // as an incompatible format rather than served under another language.
        let builder = self
            .core
            .session(chat_id)
            .session_spec(lash::SessionSpec::inherit().model(model))
            .plugin::<DemoPlugin>(DemoPluginConfig {
                db: Arc::clone(&self.db),
            });
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
        let administration = self
            .core
            .session_administration()
            .await
            .map_err(|err| AppError::internal(err.to_string()))?;
        let context = administration
            .delete_context(chat_id)
            .map_err(|err| AppError::internal(err.to_string()))?;
        LashCore::delete_session(context)
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

#[cfg(test)]
mod session_language_tests {
    use super::*;

    fn system_text(request: &lash::provider::LlmRequest) -> String {
        request
            .instructions
            .as_deref()
            .unwrap_or_default()
            .to_owned()
    }

    fn mock_model_spec() -> ModelSpec {
        ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("model spec")
    }

    async fn test_core(data_dir: &std::path::Path) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-session-language")
            .build()
            .into_handle();
        test_core_with_provider(data_dir, provider).await
    }

    async fn test_core_with_provider(
        data_dir: &std::path::Path,
        provider: lash::provider::ProviderHandle,
    ) -> LashCore {
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
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
                "agent-service-session-language",
                "test",
            ))
            .expect("core")
    }

    fn test_state(core: &LashCore, db: AppDb) -> AppStateData {
        #[cfg(feature = "restate")]
        {
            AppStateData::from_shared_db(
                core.clone(),
                core.turn_work_driver()
                    .expect("test core has a session catalog"),
                Arc::new(Mutex::new(db)),
                "mock-model".to_string(),
                None,
                AgentServiceDurability::Local,
                None,
                None,
            )
        }
        #[cfg(not(feature = "restate"))]
        {
            AppStateData::new(
                core.clone(),
                core.turn_work_driver()
                    .expect("test core has a session catalog"),
                db,
                "mock-model".to_string(),
                None,
                AgentServiceDurability::Local,
            )
        }
    }

    /// FIG-1979: a raw per-turn `dialect` key cannot re-word the host prompt.
    ///
    /// The typed per-turn options bag has no dialect field, but the merge
    /// underneath it is an untyped shallow key-extend of the host's per-turn
    /// override over the session bag, so a raw `{"dialect": ...}` key *does*
    /// reach the prompt hook's effective options. Under TypeScript-only RLM
    /// (ADR 0096) there is no second language to switch to, and the host's
    /// board copy must stay unmoved by such a key rather than being re-worded
    /// by whatever a turn asserts.
    #[tokio::test]
    async fn a_per_turn_dialect_key_cannot_re_word_the_board_prompt() {
        use lash::rlm::RlmTurnBuilderExt as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-dialect-smuggle")
            .complete({
                let seen = Arc::clone(&seen);
                move |request: lash::provider::LlmRequest| {
                    let seen = Arc::clone(&seen);
                    async move {
                        seen.lock_recover().push(system_text(&request));
                        Ok(lash::provider::LlmResponse {
                            parts: vec![lash::direct::LlmOutputPart::Text {
                                text: "<typescript>\nfinish(\"done\");\n</typescript>".to_string(),
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash::provider::LlmResponse::default()
                        })
                    }
                }
            })
            .build()
            .into_handle();
        let core = test_core_with_provider(data_dir, provider).await;

        let service = test_state(
            &core,
            AppDb::open(&data_dir.join("app-smuggle.db")).expect("app db"),
        );
        let session = service
            .open_session("smuggled-chat", mock_model_spec())
            .await
            .expect("the chat opens");

        session
            .turn(lash::TurnInput::text("play"))
            .require_finish()
            .expect("finish requirement")
            .run()
            .await
            .expect("the honest turn runs");

        // The attack: a raw per-turn override naming the retired language.
        let attack = lash::runtime::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "dialect": "lashlang" }),
        );
        let attacked = session
            .turn(lash::TurnInput::text("switch me"))
            .protocol_turn_options(attack)
            // `require_finish` writes through the same seam and merges
            // shallowly, so the attack has to survive it — otherwise this turn
            // would carry no override and the test would measure nothing.
            .require_finish()
            .expect("finish requirement");
        // That the key really does survive to the prompt hook is not assumed:
        // it is what makes this test red against a host that reads the hook's
        // effective options, and the same seam is asserted directly on the
        // public builder in the facade's own RLM session-config suite.
        attacked.run().await.expect("the attacked turn runs");
        drop(session);

        let prompts = seen.lock_recover().clone();
        assert_eq!(
            prompts.len(),
            2,
            "both turns must have reached the provider"
        );
        for prompt in &prompts {
            assert!(
                prompt.contains("outside the typescript cell"),
                "the board prompt must stay in TypeScript: {prompt}"
            );
            assert!(
                !prompt.contains("outside the lashlang block"),
                "a per-turn dialect key must not re-word the board prompt: {prompt}"
            );
        }
    }

    // `a_recorded_chat_refuses_to_reopen_under_another_dialect` is deleted:
    // TypeScript-only RLM retired the session language pin, so there is no
    // second language a reopen could disagree about (ADR 0096).

    /// A chat the service opens keeps opening.
    ///
    /// A reopen states no durable language of its own (ADR 0096), so a chat
    /// this service created is served again without a reopen tax.
    #[tokio::test]
    async fn a_chat_reopens_under_the_config_it_recorded() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let core = test_core(data_dir).await;

        let service = test_state(
            &core,
            AppDb::open(&data_dir.join("app.db")).expect("app db"),
        );
        let session = service
            .open_session("own-chat", mock_model_spec())
            .await
            .expect("first open");
        session.close().await.expect("close");

        let reopened = service
            .open_session("own-chat", mock_model_spec())
            .await
            .expect("a chat reopens under the config it recorded");
        reopened.close().await.expect("close the reopened session");
    }
}
