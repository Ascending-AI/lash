use std::sync::{Arc, Mutex};

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use lash::sync::MutexExt;
use lash::{LashCore, LashSession, TurnWorkDriver};

use crate::routes::LlmProfileChoice;
use serde_json::json;

use crate::db::AppDb;

pub(crate) type AppResult<T> = Result<T, AppError>;

#[derive(Clone)]
pub(crate) struct AppStateData {
    core: Arc<LashCore>,
    turn_work_driver: TurnWorkDriver,
    db: Arc<Mutex<AppDb>>,
    default_profile: String,
    default_profile_variant: Option<String>,
    restate: lash_restate::RestateConnection,
}

impl AppStateData {
    pub(crate) fn new(
        core: LashCore,
        db: Arc<Mutex<AppDb>>,
        default_profile: String,
        default_profile_variant: Option<String>,
        restate: lash_restate::RestateConnection,
    ) -> Self {
        let core = Arc::new(core);
        db.lock_recover().context_core = Arc::downgrade(&core);
        Self {
            turn_work_driver: core.turn_work_driver(),
            core,
            db,
            default_profile,
            default_profile_variant,
            restate,
        }
    }

    /// The core, retained for the shutdown drain (trace flush).
    pub(crate) fn core(&self) -> &LashCore {
        &self.core
    }

    pub(crate) async fn record_board_context(&self, session: &LashSession) -> AppResult<()> {
        let config = session.admin().config();
        loop {
            let chat_id = session.session_id().to_string();
            let board = self.with_db(move |db| db.chat_board(&chat_id)).await?;
            let revision = config.revision().await?;
            let outcome = config
                .apply(
                    lash::config::ConfigWrite::new(
                        format!("board-context:{}", uuid::Uuid::new_v4()),
                        revision,
                    ),
                    lash::config::ConfigTransaction::of(lash::rlm::SetRlmPromptContext {
                        context: vec![crate::board::board_prompt(&board)],
                    }),
                )
                .await?;
            match outcome {
                lash::config::ConfigTransactionOutcome::Applied { .. } => return Ok(()),
                lash::config::ConfigTransactionOutcome::Stale { .. } => continue,
                outcome => {
                    return Err(AppError::internal(format!(
                        "the board context did not apply: {outcome:?}"
                    )));
                }
            }
        }
    }

    pub(crate) async fn record_board_context_for_chat(&self, chat_id: &str) -> AppResult<()> {
        let session = self.core.session(chat_id).open().await?;
        self.record_board_context(&session).await
    }

    pub(crate) fn turn_work_driver(&self) -> &TurnWorkDriver {
        &self.turn_work_driver
    }

    pub(crate) fn default_profile(&self) -> &str {
        &self.default_profile
    }

    pub(crate) fn default_profile_variant(&self) -> Option<&str> {
        self.default_profile_variant.as_deref()
    }

    /// The Restate ingress the service's own workflows are reached through.
    pub(crate) fn restate_ingress(&self) -> lash_restate::RestateIngressClient {
        lash_restate::RestateIngressClient::new(self.restate.clone())
    }

    pub(crate) async fn open_session(
        &self,
        chat_id: &str,
        model: LlmProfileChoice,
    ) -> AppResult<LashSession> {
        // TypeScript is the sole RLM language (ADR 0096), so a chat states no
        // language at its open: there is nothing left to pin, and a bag that
        // still records the retired `dialect` field is refused by the protocol
        // as an incompatible format rather than served under another language.
        //
        // The model is creation config: the chat's session is created lazily,
        // after its app-DB chat row, and records the model then; a reopen runs
        // with what the session recorded (FIG-4099). Only `create` creates
        // (FIG-4112), so the create-or-use arm is written out: an existing
        // session keeps its recorded model, and a chat whose model changed
        // since moves its session with a config transaction.
        let board = self
            .with_db({
                let chat_id = chat_id.to_string();
                move |db| db.chat_board(&chat_id)
            })
            .await?;
        match self
            .core
            .session(chat_id)
            .create(lash::SessionCreation::root(
                // The service's default spec, running the chat's model: a
                // core keeps none, so the host states it at each creation.
                lash::SessionSpec::new(
                    model.key.clone(),
                    lash::TurnBudget::Unbounded,
                    lash::MaxToolCalls::new(1024),
                )
                .reasoning(model.reasoning.clone())
                .attachment_acceptance(Arc::new(crate::service_attachment_acceptance()))
                .plugin(
                    lash::rlm::RLM_PROTOCOL_PLUGIN_ID,
                    lash::rlm::RlmCreateExtras {
                        prompt: Some(lash::rlm::RlmPrompt {
                            context: vec![crate::board::board_prompt(&board)],
                            ..Default::default()
                        }),
                        ..Default::default()
                    },
                )
                .map_err(lash::EmbedError::from)?,
            ))
            .await
        {
            Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
            Err(error) => return Err(error.into()),
        }
        let session = self.core.session(chat_id).open().await?;
        let recorded = session
            .policy_snapshot()
            .model
            .map(|recorded| LlmProfileChoice {
                key: recorded.key().clone(),
                reasoning: recorded.reasoning,
            });
        if recorded.as_ref() != Some(&model) {
            // Written against the revision this open read, under an id that
            // names the change: a resubmission of the same change is the
            // same transaction.
            let config = session.admin().config();
            let revision = config.revision().await?;
            let outcome = config
                .apply(
                    lash::config::ConfigWrite::new(
                        format!("chat-model:{}:{:?}:{revision}", model.key, model.reasoning),
                        revision,
                    ),
                    lash::config::ConfigTransaction::of(lash::config::SetLlmProfile {
                        model: model.key,
                    })
                    .then(lash::config::SetReasoning {
                        reasoning: model.reasoning,
                    }),
                )
                .await?;
            if !matches!(
                outcome,
                lash::config::ConfigTransactionOutcome::Applied { .. }
            ) {
                return Err(AppError::internal(format!(
                    "the chat's model change did not apply: {outcome:?}"
                )));
            }
        }
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
        crate::chat_discard::discard_chat_session(&self.restate_ingress(), chat_id).await?;
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

pub(crate) mod anyhow_like {
    pub(crate) type Result<T> = std::result::Result<T, String>;
}

/// Shared test scaffolding: a real `LashCore` over temp stores plus the
/// `AppStateData` the routes are exercised against.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// The model every test chat runs: the service catalog's `mock-model`
    /// id with its provider-default reasoning.
    pub(crate) fn mock_llm_profile() -> LlmProfileChoice {
        LlmProfileChoice {
            key: lash::LlmProfileKey::new("mock-model"),
            reasoning: lash::provider::ReasoningSelection::default(),
        }
    }

    /// The Restate double a service test runs on: lash-restate's engine and
    /// services over a fresh SQLite memory store set, connected to an
    /// in-process server double. Keep it alive to the end of the test
    /// (FIG-3723); every core the test builds runs over its backend, so a
    /// later core reopens what an earlier one wrote.
    pub(crate) async fn test_double() -> lash_restate_test::RestateTestBackend {
        lash_restate_test::backend(0x0a6e_5e7c, lash_restate_test::ServerConfig::default())
            .await
            .expect("build the Restate double")
    }

    /// Serve the service's own chat-discard workflow on `double` for `core`,
    /// as the deployment's endpoint binds it beside lash's services.
    pub(crate) async fn serve_chat_discard(
        double: &lash_restate_test::RestateTestBackend,
        core: &LashCore,
    ) {
        use crate::chat_discard::AgentServiceChatDiscard as _;

        let authority = lash_restate::RestateAuthorityId::new(format!(
            "lash-restate-test-{}",
            double.server().config().seed
        ))
        .expect("the double's authority id");
        let discard = crate::chat_discard::AgentServiceChatDiscardImpl::new(
            core,
            double.connection(),
            authority,
        )
        .await;
        double
            .server()
            .register(
                restate_sdk::endpoint::Endpoint::builder()
                    .bind(discard.serve())
                    .build(),
            )
            .await
            .expect("register the chat-discard workflow on the double");
    }

    pub(crate) async fn test_core(double: &lash_restate_test::RestateTestBackend) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-test-support")
            .build()
            .into_handle();
        test_core_with_provider(double, provider).await
    }

    pub(crate) async fn test_core_with_provider(
        double: &lash_restate_test::RestateTestBackend,
        provider: lash::provider::ProviderHandle,
    ) -> LashCore {
        test_core_with_facets(
            double,
            provider,
            None,
            lash::tools::ToolSourcePolicy::Tolerate,
            None,
        )
        .await
    }

    /// A core that installs the board plugin over `db`, as the service's own
    /// core does: its sessions carry the board prompt and the board tools.
    pub(crate) async fn test_core_with_board(
        double: &lash_restate_test::RestateTestBackend,
        provider: lash::provider::ProviderHandle,
        db: &Arc<Mutex<AppDb>>,
    ) -> LashCore {
        test_core_with_facets(
            double,
            provider,
            None,
            lash::tools::ToolSourcePolicy::Tolerate,
            Some(Arc::clone(db)),
        )
        .await
    }

    /// A core that refuses to serve a chat whose persisted tools have no
    /// source here — the unattended-deployment posture (FIG-3367).
    pub(crate) async fn test_core_requiring_tool_sources(
        double: &lash_restate_test::RestateTestBackend,
    ) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-test-support")
            .build()
            .into_handle();
        test_core_with_facets(
            double,
            provider,
            None,
            lash::tools::ToolSourcePolicy::Require,
            None,
        )
        .await
    }

    pub(crate) async fn test_core_with_facets(
        double: &lash_restate_test::RestateTestBackend,
        provider: lash::provider::ProviderHandle,
        tools: Option<Arc<dyn lash::tools::ToolProvider>>,
        tool_source_policy: lash::tools::ToolSourcePolicy,
        board: Option<Arc<Mutex<AppDb>>>,
    ) -> LashCore {
        let backend = double.lash_backend();
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash::rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            std::sync::Arc::new(lash_protocol_rlm::TypescriptDialect),
            &backend,
        );
        let mut builder = LashCore::rlm_builder(backend, factory)
            .tool_source_policy(tool_source_policy)
            .llm_profiles(Arc::new(crate::OpenRouterLlmProfiles { provider }));
        if let Some(tools) = tools {
            builder = builder.tools(tools);
        }
        if let Some(db) = board {
            builder = builder.plugin(Arc::new(crate::demo_plugin::DemoPluginFactory::new(db)));
        }
        builder
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "agent-service-test-support",
                "test",
            ))
            .expect("core")
    }

    /// A core with an extra host tool source, for seeding a chat whose
    /// checkpoint records a tool a later core will not carry.
    pub(crate) async fn test_core_with_tools(
        double: &lash_restate_test::RestateTestBackend,
        tools: Arc<dyn lash::tools::ToolProvider>,
    ) -> LashCore {
        let provider = lash::testing::TestProvider::builder()
            .kind("agent-service-test-support")
            .build()
            .into_handle();
        test_core_with_facets(
            double,
            provider,
            Some(tools),
            lash::tools::ToolSourcePolicy::Tolerate,
            None,
        )
        .await
    }

    /// The service state over `core`, reaching Restate through `double`.
    pub(crate) fn test_state(
        double: &lash_restate_test::RestateTestBackend,
        core: &LashCore,
        db: AppDb,
    ) -> AppStateData {
        AppStateData::new(
            core.clone(),
            Arc::new(Mutex::new(db)),
            "mock-model".to_string(),
            None,
            double.connection(),
        )
    }
}

#[cfg(test)]
mod session_language_tests {
    use super::test_support::{
        mock_llm_profile, test_core, test_core_requiring_tool_sources, test_core_with_board,
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

    /// A raw per-turn dialect change is refused before provider dispatch,
    /// preserves the recorded TypeScript dialect, and leaves subsequent
    /// legal turns' prompts in TypeScript (FIG-1979, FIG-4463).
    #[tokio::test]
    async fn a_per_turn_dialect_key_cannot_re_word_the_prompt() {
        use lash::rlm::RlmSendBuilderExt as _;

        let temp = tempfile::tempdir().expect("tempdir");
        let data_dir = temp.path();
        let double = crate::state::test_support::test_double().await;
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
        let db = Arc::new(Mutex::new(
            AppDb::open(&data_dir.join("app-smuggle.db")).expect("app db"),
        ));
        let core = test_core_with_board(&double, provider, &db).await;

        let service = AppStateData::new(
            core.clone(),
            db,
            "mock-model".to_string(),
            None,
            double.connection(),
        );
        let session = service
            .open_session("smuggled-chat", mock_llm_profile())
            .await
            .expect("the chat opens");

        session
            .send(lash::TurnInput::text("play"))
            .require_finish()
            .expect("finish requirement")
            .output()
            .await
            .expect("the honest turn runs");

        let recorded: lash::rlm::RlmRecordedConfig = session
            .read_view()
            .protocol_turn_options()
            .decode()
            .expect("the recorded RLM namespace decodes");
        assert_eq!(recorded.dialect.as_deref(), Some("typescript"));
        assert_eq!(
            seen.lock_recover().len(),
            1,
            "the honest turn reached the provider"
        );

        // The attack: a raw per-turn override naming the retired language.
        let attack = lash::runtime::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "dialect": "lashlang" }),
        );
        let attacked = session
            .send(lash::TurnInput::text("switch me"))
            .protocol_turn_options(attack);
        let refused = attacked
            .output()
            .await
            .expect_err("the RLM owner refuses a per-turn dialect change");
        assert!(
            matches!(
                refused,
                lash::EmbedError::Runtime(ref error)
                    if error.code == lash::runtime::RuntimeErrorCode::RunShapeRefused
                        && matches!(
                            error.run_shape_refusal(),
                            Some(lash::RunShapeRefusal::Owner { refusal })
                                if refusal.owner == lash::rlm::RLM_PROTOCOL_PLUGIN_ID
                        )
            ),
            "the dialect attack must be refused as a run shape: {refused:?}"
        );
        assert_eq!(
            seen.lock_recover().len(),
            1,
            "the refused attack must not reach the provider"
        );
        let after_attack: lash::rlm::RlmRecordedConfig = session
            .read_view()
            .protocol_turn_options()
            .decode()
            .expect("the recorded RLM namespace still decodes");
        assert_eq!(
            after_attack, recorded,
            "the attack must preserve the recorded config"
        );

        session
            .send(lash::TurnInput::text("keep playing"))
            .require_finish()
            .expect("finish requirement")
            .output()
            .await
            .expect("a legal turn still runs after the refused attack");
        drop(session);

        let prompts = seen.lock_recover().clone();
        assert_eq!(
            prompts.len(),
            2,
            "only the honest and subsequent legal turns reach the provider"
        );
        for prompt in &prompts {
            assert!(
                prompt.contains("## TypeScript execution"),
                "the prompt must stay in TypeScript: {prompt}"
            );
            assert!(
                !prompt.contains("<lashlang>"),
                "a per-turn dialect key must not re-word the prompt: {prompt}"
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
        let double = crate::state::test_support::test_double().await;
        let db_path = data_dir.join("app-tool-loss.db");
        let chat_id = {
            let mut db = AppDb::open(&db_path).expect("app db");
            db.create_chat("tool loss", "mock-model", None)
                .expect("create chat")
                .id
        };

        // Seed: a core that carries the source persists the tool in the
        // session's checkpoint.
        let seeding_core = test_core_with_tools(&double, Arc::new(SeedTools)).await;
        seeding_core
            .session(chat_id.clone())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                mock_llm_profile().key,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )))
            .await
            .expect("seed create");
        let seeded = seeding_core
            .session(chat_id.clone())
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
        let core = test_core(&double).await;
        let service = test_state(&double, &core, AppDb::open(&db_path).expect("app db"));
        let session = service
            .open_session(&chat_id, mock_llm_profile())
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
            .open_session(&chat_id, mock_llm_profile())
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
        .expect("valid declared tool schemas")
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
        let double = crate::state::test_support::test_double().await;
        let db_path = data_dir.join("app-require.db");
        let chat_id = {
            let mut db = AppDb::open(&db_path).expect("app db");
            db.create_chat("require", "mock-model", None)
                .expect("create chat")
                .id
        };

        let seeding_core = test_core_with_tools(&double, Arc::new(SeedTools)).await;
        seeding_core
            .session(chat_id.clone())
            .create(lash::SessionCreation::root(lash::SessionSpec::new(
                mock_llm_profile().key,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )))
            .await
            .expect("seed create");
        let seeded = seeding_core
            .session(chat_id.clone())
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

        let core = test_core_requiring_tool_sources(&double).await;
        let service = test_state(&double, &core, AppDb::open(&db_path).expect("app db"));
        let refusal = match service.open_session(&chat_id, mock_llm_profile()).await {
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
        let double = crate::state::test_support::test_double().await;
        let core = test_core(&double).await;

        let service = test_state(
            &double,
            &core,
            AppDb::open(&data_dir.join("app.db")).expect("app db"),
        );
        let session = service
            .open_session("own-chat", mock_llm_profile())
            .await
            .expect("first open");
        session.close().await.expect("close");

        let reopened = service
            .open_session("own-chat", mock_llm_profile())
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
