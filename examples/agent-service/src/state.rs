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
    ) -> Self {
        Self {
            core,
            turn_work_driver,
            db,
            default_model,
            default_model_variant,
            durability,
            restate_ingress_url,
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

    pub(crate) async fn open_session(
        &self,
        chat_id: &str,
        model: ModelSpec,
    ) -> AppResult<LashSession> {
        // TypeScript is the sole RLM language (ADR 0096), so a chat states no
        // language at its open: there is nothing left to pin, and a bag that
        // still records the retired `dialect` field is refused by the protocol
        // as an incompatible format rather than served under another language.
        let session = self
            .core
            .session(chat_id)
            .session_spec(lash::SessionSpec::inherit().model(model))
            .plugin::<DemoPlugin>(DemoPluginConfig {
                db: Arc::clone(&self.db),
            })
            .open()
            .await?;
        self.record_tool_loss_notice(chat_id, &session).await?;
        Ok(session)
    }

    /// Tell this chat's user when the reopened session lost a tool.
    ///
    /// An open is the one moment the service learns that a persisted tool has
    /// no live source (FIG-3367). The report is a typed value, so the notice
    /// names the exact tool ids and goes into the transcript the user reads
    /// rather than into the service log. Only lost members are rendered: a
    /// parked opt-out is a tool the user already turned off, and a superseded
    /// identity is the same capability under a new id.
    pub(crate) async fn record_tool_loss_notice(
        &self,
        chat_id: &str,
        session: &LashSession,
    ) -> AppResult<()> {
        let Some(report) = session.tool_restore_report().await else {
            return Ok(());
        };
        if !report.has_lost_members() {
            return Ok(());
        }
        let notice = tool_loss_notice_text(&report);
        let chat_id = chat_id.to_string();
        self.with_db(move |db| {
            // One notice per distinct loss: every request opens the chat, and
            // a transcript that repeats the same warning per poll is noise.
            let already_told = db
                .list_messages(&chat_id)?
                .iter()
                .any(|message| message.body.text() == notice);
            if !already_told {
                db.insert_message(&chat_id, "system", &notice)?;
            }
            Ok(())
        })
        .await
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
        let administration = self.core.session_administration().await;
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

/// The user-facing sentence for a tool-restore report that lost members.
pub(crate) fn tool_loss_notice_text(report: &lash::tools::ToolRestoreReport) -> String {
    let lost = report
        .lost_members
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "Some tools this chat had are unavailable: {lost}. The chat still works; \
         those tools return when their source does."
    )
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
    /// A retryable refusal (the session briefly busy) stays retryable over
    /// HTTP: it answers 503, never a 500 a client would not resend.
    fn from(err: lash::EmbedError) -> Self {
        if err.is_retryable() {
            return Self {
                status: StatusCode::SERVICE_UNAVAILABLE,
                message: err.to_string(),
            };
        }
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

/// Shared test scaffolding: a real `LashCore` over temp stores plus the
/// `AppStateData` the routes are exercised against.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    pub(crate) fn mock_model_spec() -> ModelSpec {
        ModelSpec::builder("mock-model")
            .context_window_tokens(200_000)
            .build()
            .expect("model spec")
    }

    pub(crate) async fn test_core(data_dir: &std::path::Path) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-test-support")
            .build()
            .into_handle();
        test_core_with_provider(data_dir, provider).await
    }

    pub(crate) async fn test_core_with_provider(
        data_dir: &std::path::Path,
        provider: lash::provider::ProviderHandle,
    ) -> LashCore {
        test_core_with_facets(
            data_dir,
            provider,
            None,
            lash::tools::ToolSourcePolicy::Tolerate,
        )
        .await
    }

    /// A core that refuses to serve a chat whose persisted tools have no
    /// source here — the unattended-deployment posture (FIG-3367).
    pub(crate) async fn test_core_requiring_tool_sources(data_dir: &std::path::Path) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-test-support")
            .build()
            .into_handle();
        test_core_with_facets(
            data_dir,
            provider,
            None,
            lash::tools::ToolSourcePolicy::Require,
        )
        .await
    }

    pub(crate) async fn test_core_with_facets(
        data_dir: &std::path::Path,
        provider: lash::provider::ProviderHandle,
        tools: Option<Arc<dyn lash::tools::ToolProvider>>,
        tool_source_policy: lash::tools::ToolSourcePolicy,
    ) -> LashCore {
        let backend = test_backend(data_dir).await;
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            &backend.clone().into(),
        );
        let mut builder =
            LashCore::rlm_builder(backend.into(), lash::TurnBudget::Unbounded, factory)
                .tool_source_policy(tool_source_policy)
                .provider(provider);
        if let Some(tools) = tools {
            builder = builder.tools(tools);
        }
        builder
            .model(mock_model_spec())
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-test-support",
                "test",
            ))
            .expect("core")
    }

    /// The Local durability backend the service opens: a file
    /// `SqliteBackend` on the data directory's sessions root.
    pub(crate) async fn test_backend(
        data_dir: &std::path::Path,
    ) -> Arc<lash_sqlite_store::SqliteBackend> {
        Arc::new(
            lash_sqlite_store::SqliteBackend::open(data_dir.join("lash-sessions"))
                .await
                .expect("open the Local SQLite backend"),
        )
    }

    /// A core with an extra host tool source, for seeding a chat whose
    /// checkpoint records a tool a later core will not carry.
    pub(crate) async fn test_core_with_tools(
        data_dir: &std::path::Path,
        tools: Arc<dyn lash::tools::ToolProvider>,
    ) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-test-support")
            .build()
            .into_handle();
        test_core_with_facets(
            data_dir,
            provider,
            Some(tools),
            lash::tools::ToolSourcePolicy::Tolerate,
        )
        .await
    }

    pub(crate) fn test_state(core: &LashCore, db: AppDb) -> AppStateData {
        #[cfg(feature = "restate")]
        {
            AppStateData::from_shared_db(
                core.clone(),
                core.turn_work_driver(),
                Arc::new(Mutex::new(db)),
                "mock-model".to_string(),
                None,
                AgentServiceDurability::Local,
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
            )
        }
    }
}

#[cfg(test)]
mod session_language_tests {
    use super::test_support::{
        mock_model_spec, test_core, test_core_requiring_tool_sources, test_core_with_provider,
        test_core_with_tools, test_state,
    };
    use super::*;

    fn system_text(request: &lash::provider::LlmRequest) -> String {
        request
            .instructions
            .as_deref()
            .unwrap_or_default()
            .to_owned()
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

    /// A reopen that lost a tool tells this chat's user, by tool id, once
    /// (FIG-3367).
    ///
    /// The seed core carries a host tool source; the serving core does not, so
    /// the reopen's restore report has a lost member. Dropping the report — or
    /// rendering it only as a log line — leaves the transcript without the
    /// notice and fails this test.
    #[tokio::test]
    async fn a_reopen_that_lost_a_tool_tells_the_user_once() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let db_path = data_dir.join("app-tool-loss.db");
        let chat_id = {
            let mut db = AppDb::open(&db_path).expect("app db");
            db.create_chat("tool loss", "mock-model", None)
                .expect("create chat")
                .id
        };

        // Seed: a core that carries the source persists the tool in the
        // session's checkpoint.
        let seeding_core = test_core_with_tools(data_dir, Arc::new(SeedTools)).await;
        let seeded = seeding_core
            .session(chat_id.clone())
            .session_spec(lash::SessionSpec::inherit().model(mock_model_spec()))
            .open()
            .await
            .expect("seed open");
        seeded
            .admin()
            .state()
            .append_messages(vec![lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::Assistant,
                "seeded while the tool source was present",
            )])
            .await
            .expect("append a committed message while the tool source is present");
        seeded.close().await.expect("close the seeded session");

        // Serve: the service's own core has no such source.
        let core = test_core(data_dir).await;
        let service = test_state(&core, AppDb::open(&db_path).expect("app db"));
        let session = service
            .open_session(&chat_id, mock_model_spec())
            .await
            .expect("the chat still opens");
        session.close().await.expect("close");

        async fn system_notices(service: &AppStateData, chat_id: &str) -> Vec<String> {
            let chat_id = chat_id.to_string();
            service
                .with_db(move |db| db.list_messages(&chat_id))
                .await
                .expect("list messages")
                .into_iter()
                .filter(|message| message.role() == "system")
                .map(|message| message.text().to_string())
                .collect()
        }

        let told = system_notices(&service, &chat_id).await;
        assert_eq!(told.len(), 1, "the user is told exactly once, got {told:?}");
        assert!(
            told[0].contains("tool:agent_service_seed_lookup"),
            "the notice names the lost tool id: {}",
            told[0]
        );

        // A second open of the same chat does not repeat the notice.
        let again = service
            .open_session(&chat_id, mock_model_spec())
            .await
            .expect("second open");
        again.close().await.expect("close");
        assert_eq!(
            system_notices(&service, &chat_id).await.len(),
            1,
            "one notice per distinct loss, not one per request"
        );
    }

    struct SeedTools;

    fn seed_tool_definition() -> lash::tools::ToolDefinition {
        lash::tools::ToolDefinition::raw(
            "tool:agent_service_seed_lookup",
            "agent_service_seed_lookup",
            "a host tool only the seeding core carries",
            lash::tools::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "object", "additionalProperties": true }),
        )
    }

    #[async_trait::async_trait]
    impl lash::tools::ToolProvider for SeedTools {
        fn tool_manifests(&self) -> Vec<lash::tools::ToolManifest> {
            vec![seed_tool_definition().manifest()]
        }

        fn resolve_contract(&self, name: &str) -> Option<Arc<lash::tools::ToolContract>> {
            (name == "agent_service_seed_lookup")
                .then(|| Arc::new(seed_tool_definition().contract()))
        }

        async fn execute(
            &self,
            _call: lash::tools::ToolCall<'_>,
        ) -> lash::tools::ToolAttemptOutcome {
            lash::tools::ToolOutcome::ok(serde_json::json!({ "ok": true })).into()
        }
    }

    /// The remote host under Require: a chat whose persisted tool has no
    /// source here is refused rather than served degraded (FIG-3367).
    #[tokio::test]
    async fn a_require_core_refuses_to_serve_a_chat_that_lost_a_tool() {
        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let db_path = data_dir.join("app-require.db");
        let chat_id = {
            let mut db = AppDb::open(&db_path).expect("app db");
            db.create_chat("require", "mock-model", None)
                .expect("create chat")
                .id
        };

        let seeding_core = test_core_with_tools(data_dir, Arc::new(SeedTools)).await;
        let seeded = seeding_core
            .session(chat_id.clone())
            .session_spec(lash::SessionSpec::inherit().model(mock_model_spec()))
            .open()
            .await
            .expect("seed open");
        seeded
            .admin()
            .state()
            .append_messages(vec![lash::plugins::PluginMessage::text(
                lash::messages::MessageRole::Assistant,
                "seeded while the tool source was present",
            )])
            .await
            .expect("append a committed message while the tool source is present");
        seeded.close().await.expect("close the seeded session");

        let core = test_core_requiring_tool_sources(data_dir).await;
        let service = test_state(&core, AppDb::open(&db_path).expect("app db"));
        let refusal = match service.open_session(&chat_id, mock_model_spec()).await {
            Ok(_) => panic!("a Require core must refuse a chat that lost a tool"),
            Err(error) => error.message,
        };
        assert!(
            refusal.contains("tool:agent_service_seed_lookup"),
            "the refusal names the missing tool: {refusal}"
        );
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

#[cfg(test)]
mod retryable_refusal_tests {
    use super::*;

    /// A refusal the same request would get past once the session's lane is
    /// free stays retryable over HTTP (FIG-3831): it answers 503, never the
    /// 500 a client would not resend; any other lash error stays a 500.
    #[test]
    fn a_retryable_lash_refusal_answers_service_unavailable() {
        let busy = lash::EmbedError::Runtime(lash::runtime::RuntimeError::new(
            lash::runtime::RuntimeErrorCode::SessionExecutionLaneBusy,
            "the session's execution lane is held",
        ));
        assert!(busy.is_retryable());
        assert_eq!(AppError::from(busy).status, StatusCode::SERVICE_UNAVAILABLE);

        let unknown = lash::EmbedError::UnknownSession {
            session_id: lash::SessionId::from("missing"),
        };
        assert!(!unknown.is_retryable());
        assert_eq!(
            AppError::from(unknown).status,
            StatusCode::INTERNAL_SERVER_ERROR
        );
    }
}
