use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::{Value, json};
use tokio::sync::Mutex;

use lash_plugin_plan_mode::{PlanModePluginConfig, PlanModePluginFactory};

use lash_core::plugin::runtime_host::{
    SessionGraphService, SessionLifecycleService, SessionStateService,
};
use lash_core::plugin::{
    PluginDirective, PluginError, PromptHookContext, ToolCallHookContext, ToolCatalogContext,
    ToolResultHookContext,
};
use lash_core::runtime::RuntimeSessionState;
use lash_core::{
    AssembledTurn, MessageRole, PluginHost, SessionCreateRequest, SessionHandle, SessionPolicy,
    SessionReadView, SessionSnapshot, ToolDefinition, ToolRegistry, ToolResult, TurnHookContext,
    TurnResultHookContext,
};

use lash_core::testing::{MockSessionManager, mock_assembled_turn};
use lash_tool_support::{
    LashlangToolBinding, StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt,
};

fn unavailable_processes() -> Arc<dyn lash_core::ProcessService> {
    Arc::new(lash_core::UnavailableProcessService)
}

fn sessions<T>(manager: &Arc<T>) -> Arc<dyn SessionStateService>
where
    T: SessionStateService + 'static,
{
    manager.clone()
}

fn session_lifecycle<T>(manager: &Arc<T>) -> Arc<dyn SessionLifecycleService>
where
    T: SessionLifecycleService + 'static,
{
    manager.clone()
}

fn session_graph<T>(manager: &Arc<T>) -> Arc<dyn SessionGraphService>
where
    T: SessionGraphService + 'static,
{
    manager.clone()
}

#[async_trait::async_trait]
trait PlanTestHostCore: Send + Sync {
    async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError>;
    async fn snapshot_session(&self, session_id: &str) -> Result<SessionSnapshot, PluginError>;
    async fn tool_catalog(&self, session_id: &str) -> Result<Vec<serde_json::Value>, PluginError>;
    async fn tool_state(&self, _session_id: &str) -> Result<lash_core::ToolState, PluginError> {
        Err(PluginError::Session(
            "tool state is unavailable in this session".to_string(),
        ))
    }
    async fn apply_tool_state(
        &self,
        _session_id: &str,
        _snapshot: lash_core::ToolState,
    ) -> Result<u64, PluginError> {
        Err(PluginError::Session(
            "tool state mutation is unavailable in this session".to_string(),
        ))
    }
    async fn create_session(
        &self,
        request: SessionCreateRequest,
    ) -> Result<SessionHandle, PluginError>;
    async fn close_session(&self, session_id: &str) -> Result<(), PluginError>;
    async fn prompt_user(
        &self,
        _request: lash_plugin_plan_mode::PlanModePromptRequest,
    ) -> Result<lash_plugin_plan_mode::PlanModePromptResponse, PluginError> {
        Err(PluginError::Session("prompt unavailable".to_string()))
    }
}

macro_rules! impl_plan_test_host {
    ($ty:ty) => {
        #[async_trait::async_trait]
        impl SessionStateService for $ty {
            async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
                PlanTestHostCore::snapshot_current(self).await
            }
            async fn snapshot_session(
                &self,
                session_id: &str,
            ) -> Result<SessionSnapshot, PluginError> {
                PlanTestHostCore::snapshot_session(self, session_id).await
            }
            async fn tool_catalog(
                &self,
                session_id: &str,
            ) -> Result<Vec<serde_json::Value>, PluginError> {
                PlanTestHostCore::tool_catalog(self, session_id).await
            }
            async fn tool_state(
                &self,
                session_id: &str,
            ) -> Result<lash_core::ToolState, PluginError> {
                PlanTestHostCore::tool_state(self, session_id).await
            }
            async fn apply_tool_state(
                &self,
                session_id: &str,
                snapshot: lash_core::ToolState,
            ) -> Result<u64, PluginError> {
                PlanTestHostCore::apply_tool_state(self, session_id, snapshot).await
            }
        }

        #[async_trait::async_trait]
        impl SessionLifecycleService for $ty {
            async fn create_session(
                &self,
                request: SessionCreateRequest,
            ) -> Result<SessionHandle, PluginError> {
                PlanTestHostCore::create_session(self, request).await
            }

            async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
                PlanTestHostCore::close_session(self, session_id).await
            }
        }

        #[async_trait::async_trait]
        impl SessionGraphService for $ty {}

        #[async_trait::async_trait]
        impl lash_plugin_plan_mode::PlanModePrompt for $ty {
            async fn prompt_user(
                &self,
                request: lash_plugin_plan_mode::PlanModePromptRequest,
            ) -> Result<lash_plugin_plan_mode::PlanModePromptResponse, PluginError> {
                PlanTestHostCore::prompt_user(self, request).await
            }
        }
    };
}

fn mock_snapshot(run_session_id: &str) -> SessionSnapshot {
    SessionSnapshot {
        session_id: "root".to_string(),
        policy: SessionPolicy {
            session_id: Some(run_session_id.to_string()),
            ..Default::default()
        },
        ..Default::default()
    }
}

fn mock_read_view(run_session_id: &str) -> SessionReadView {
    let state = RuntimeSessionState::from_snapshot(mock_snapshot(run_session_id));
    SessionReadView::from_persisted_state(&state)
}

fn test_tool(name: &str, description: &str) -> ToolDefinition {
    ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
}

struct PlanModeTestTools;

#[async_trait::async_trait]
impl StaticToolExecute for PlanModeTestTools {
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> ToolResult {
        ToolResult::err_fmt(format_args!("unexpected tool call: {}", call.name))
    }
}

fn plan_mode_test_provider() -> StaticToolProvider<PlanModeTestTools> {
    StaticToolProvider::new(vec![plan_mode_test_tool_definition()], PlanModeTestTools)
}

fn plan_mode_test_tool_definition() -> ToolDefinition {
    test_tool("plan_exit", "Ask whether to exit plan mode.")
        .with_examples(vec!["await plan.exit({})?".to_string()])
        .with_lashlang_binding(LashlangToolBinding::new(["plan"], "exit"))
}

fn mock_session_manager(run_session_id: &str) -> MockSessionManager {
    MockSessionManager::default()
        .with_snapshot(mock_snapshot(run_session_id))
        .with_turn(mock_assembled_turn(run_session_id, ""))
        .with_tool_registry(
            ToolRegistry::from_tool_provider(Arc::new(plan_mode_test_provider()))
                .expect("plan mode tool registry"),
        )
}

fn showcase_plan_exit_for_direct_execute(session: &lash_core::plugin::PluginSession) {
    // `plan_exit` is a catalog member by default; this is a no-op kept for
    // call-site clarity in the direct-execute tests.
    let _ = session;
}

fn plan_mode_env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct CurrentDirGuard {
    original: PathBuf,
}

impl CurrentDirGuard {
    fn set(path: &Path) -> Self {
        let original = std::env::current_dir().expect("current dir");
        std::env::set_current_dir(path).expect("set current dir");
        Self { original }
    }
}

impl Drop for CurrentDirGuard {
    fn drop(&mut self) {
        std::env::set_current_dir(&self.original).expect("restore current dir");
    }
}

fn plan_file_path(root: &Path, run_session_id: &str) -> PathBuf {
    root.join(".lash")
        .join("plans")
        .join(format!("{run_session_id}.md"))
}

fn ready_plan_markdown() -> &'static str {
    r#"# Plan

## Goal
- Update the plan-mode behavior cleanly.

## Steps
- Tighten the allowlist.
- Validate the plan before the frame switch.
- Execute the implementation turn after approval.

## Files
- crates/lash/src/plugin_builtin/plan_mode.rs
- host-ui/src/plan_panel.rs

## Risks
- Status/panel sync could drift if toggle handling is incomplete.

## Verification
- cargo test -p lash --lib
- cargo test -p host-ui
"#
}

fn empty_turn(session_id: &str) -> AssembledTurn {
    mock_assembled_turn(session_id, "")
}

fn plan_mode_host(plan_factory: PlanModePluginFactory) -> PluginHost {
    let mut factories = lash_core::testing::test_standard_protocol_factories();
    factories.push(Arc::new(plan_factory));
    PluginHost::new(factories)
}

async fn run_plan_command<T>(
    session: &lash_core::PluginSession,
    name: &str,
    args: Value,
    manager: &Arc<T>,
) -> Value
where
    T: SessionStateService + SessionLifecycleService + SessionGraphService + 'static,
{
    let (_, outcome) = session
        .run_plugin_command_value(
            name,
            args,
            None,
            true,
            sessions(manager),
            session_lifecycle(manager),
            session_graph(manager),
            unavailable_processes(),
        )
        .await
        .expect(name);
    outcome.output
}

#[tokio::test]
async fn plan_mode_plugin_enable_toggle_and_restore_round_trip() {
    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let host = plan_mode_host(PlanModePluginFactory::default());
    let session = host.build_session("root", None).expect("session");
    let manager = Arc::new(mock_session_manager("run-session"));

    let enabled_value = run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;
    assert_eq!(
        enabled_value.get("enabled").and_then(|v| v.as_bool()),
        Some(true)
    );
    assert_eq!(
        enabled_value
            .get("plan_path")
            .and_then(|value| value.as_str()),
        Some(".lash/plans/run-session.md")
    );
    assert!(plan_file_path(temp.path(), "run-session").is_file());

    let snapshot = session.snapshot().expect("snapshot");
    let restored = host
        .build_session("restored", Some(&snapshot))
        .expect("restored");
    let restored_toggle_value =
        run_plan_command(&restored, "plan_mode.toggle", json!({}), &manager).await;
    assert_eq!(
        restored_toggle_value
            .get("enabled")
            .and_then(|v| v.as_bool()),
        Some(false)
    );

    restored
        .restore(&lash_core::PluginSessionSnapshot::default())
        .expect("reset restore");
    let reset_manager = Arc::new(mock_session_manager("run-session"));
    let reset_toggle_value =
        run_plan_command(&restored, "plan_mode.toggle", json!({}), &reset_manager).await;
    assert_eq!(
        reset_toggle_value.get("enabled").and_then(|v| v.as_bool()),
        Some(true)
    );
}

#[tokio::test]
async fn plan_mode_toggles_plan_exit_catalog_membership() {
    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let host = plan_mode_host(PlanModePluginFactory::default());
    let session = host.build_session("root", None).expect("session");
    let manager = Arc::new(mock_session_manager("run-session"));
    let manager_host = manager.clone();

    let plan_exit_is_member = |session: &Arc<lash_core::plugin::PluginSession>| {
        session
            .resolved_tool_catalog("root")
            .expect("catalog")
            .has_callable_tool("plan_exit")
    };

    // Disabled by default: the catalog contribution removes plan_exit.
    assert!(!plan_exit_is_member(&session));

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager_host).await;
    assert!(plan_exit_is_member(&session));

    run_plan_command(&session, "plan_mode.disable", json!({}), &manager_host).await;
    assert!(!plan_exit_is_member(&session));
}

#[tokio::test]
async fn plan_mode_plugin_injects_guidance_and_blocks_implementation_tools() {
    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let host = plan_mode_host(PlanModePluginFactory::default());
    let session = host.build_session("root", None).expect("session");
    let manager = Arc::new(mock_session_manager("run-session"));

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    let before_turn = session
        .before_turn(TurnHookContext {
            session_id: "root".to_string(),
            state: mock_read_view("run-session"),
            sessions: sessions(&manager),
            turn_context: lash_core::TurnContext::default(),
        })
        .await
        .expect("before_turn");
    assert!(before_turn.iter().any(|emitted| matches!(
        &emitted.value,
        PluginDirective::EnqueueMessages { messages }
            if messages.iter().any(|message|
                message.role == MessageRole::System
                    && message.content.contains("Plan mode:")
                    && message.content.contains(".lash/plans/run-session.md")
                    && message.content.contains("plan.exit")
                    && message.content.contains("single source of truth")
                    && message.content.contains("host can surface the file path"))
    )));
    assert!(before_turn.iter().any(|emitted| {
        emitted.plugin_id == "plan_mode"
            && matches!(
                &emitted.value,
                PluginDirective::EmitRuntimeEvents { events }
                    if events.iter().any(|event| matches!(
                        event,
                        lash_core::plugin::PluginRuntimeEvent::Custom { name, payload }
                            if name == "plan_mode.state"
                                && payload.get("enabled").and_then(|value| value.as_bool()) == Some(true)
                                && payload.get("plan_path").and_then(|value| value.as_str()) == Some(".lash/plans/run-session.md")
                    ))
            )
    }));
    assert!(plan_file_path(temp.path(), "run-session").is_file());

    let blocked_exec = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "exec_command".to_string(),
            json!({"cmd":"cargo test -q"}),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(!blocked_exec.is_empty());

    let allowed = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "read_file".to_string(),
            json!({"path":"src/main.rs"}),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(allowed.is_empty());

    let tools = vec![
        test_tool("search_tools", "Discover tools"),
        test_tool("read_file", "Read files"),
        test_tool("search_web", "Search the web"),
        test_tool("edit", "Edit files"),
        test_tool("write", "Write files"),
        // Not in the plan-mode allow-list: must be removed from the catalog.
        test_tool("exec_command", "Run a command"),
        test_tool("plan_exit", "Exit plan mode"),
    ];
    let contracts = tools
        .iter()
        .map(|tool| (tool.name().to_string(), Arc::new(tool.contract())))
        .collect::<std::collections::BTreeMap<_, _>>();
    let manifests = tools.into_iter().map(|tool| tool.manifest()).collect();
    let surface = session
        .resolve_tool_catalog(ToolCatalogContext {
            session_id: "root".to_string(),
            tools: manifests,
            resolve_contract: Some(Arc::new(move |name| contracts.get(name).cloned())),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
        })
        .expect("tool catalog");
    // In plan mode, membership is the execution gate: allowed tools stay members
    // (the default allow-list includes search_web/read_file/edit/write), and
    // plan_exit stays a member.
    assert!(surface.has_callable_tool("search_web"));
    assert!(surface.has_callable_tool("read_file"));
    assert!(surface.has_callable_tool("plan_exit"));
    assert!(surface.has_callable_tool("edit"));
    assert!(surface.has_callable_tool("write"));
    assert!(!surface.has_callable_tool("exec_command"));
    // The plan-mode note is now an ordinary prompt contribution.
    let prompt = session
        .collect_prompt_contributions(PromptHookContext {
            session_id: "root".to_string(),
            sessions: sessions(&manager),
            state: mock_read_view("run-session"),
            protocol_turn_options: Default::default(),
            turn_context: lash_core::TurnContext::default(),
        })
        .await
        .expect("prompt contributions");
    assert!(
        prompt
            .iter()
            .any(|c| c.content.contains(".lash/plans/run-session.md"))
    );

    let read_allowed = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "read_file".to_string(),
            json!({
                "path":"src/main.rs"
            }),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(read_allowed.is_empty());

    let web_allowed = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "search_web".to_string(),
            json!({
                "query":"surrealdb datetime best practices"
            }),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(web_allowed.is_empty());

    let plan_edit_allowed = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "edit".to_string(),
            json!({
                "path": ".lash/plans/run-session.md",
                "edits": [{ "oldText": "TBD", "newText": "Write the implementation plan" }]
            }),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(plan_edit_allowed.is_empty());

    let plan_write_allowed = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "write".to_string(),
            json!({
                "path": ".lash/plans/run-session.md",
                "content": "# Plan\n\n## Goal\n- Write the implementation plan\n"
            }),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(plan_write_allowed.is_empty());

    let repo_edit_blocked = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "edit".to_string(),
            json!({
                "path": "src/main.rs",
                "edits": [{ "oldText": "fn main() {}", "newText": "fn main() { run(); }" }]
            }),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(repo_edit_blocked.iter().any(|emitted| matches!(
        &emitted.value,
        PluginDirective::AbortTurn { code, message }
            if code == "plan_mode_tool_blocked"
                && message.contains(".lash/plans/run-session.md")
    )));

    run_plan_command(&session, "plan_mode.disable", json!({}), &manager).await;
    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    let checkpoint = session
        .at_checkpoint(lash_core::CheckpointHookContext {
            session_id: "root".to_string(),
            checkpoint: lash_core::CheckpointKind::AfterWork,
            state: mock_read_view("run-session"),
            sessions: sessions(&manager),
            session_lifecycle: session_lifecycle(&manager),
            session_graph: session_graph(&manager),
        })
        .await
        .expect("checkpoint");
    assert!(checkpoint.iter().any(|emitted| matches!(
        &emitted.value,
        PluginDirective::EnqueueMessages { messages }
            if messages.iter().any(|message|
                message.content.contains(".lash/plans/run-session.md")
                    && message.content.contains("plan.exit")
                    && message.content.contains("single source of truth")
                    && message.content.contains("host can surface the file path"))
    )));

    session
        .after_turn(TurnResultHookContext {
            session_id: "root".to_string(),
            turn: Arc::new(lash_core::TurnResultSummary::from_assembled(&empty_turn(
                "root",
            ))),
            sessions: sessions(&manager),
        })
        .await
        .expect("after_turn");
}

#[tokio::test]
async fn plan_mode_does_not_reinject_entry_guidance_on_later_turns() {
    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let host = plan_mode_host(PlanModePluginFactory::default());
    let session = host.build_session("root", None).expect("session");
    let manager = Arc::new(mock_session_manager("run-session"));

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    let first_before_turn = session
        .before_turn(TurnHookContext {
            session_id: "root".to_string(),
            state: mock_read_view("run-session"),
            sessions: sessions(&manager),
            turn_context: lash_core::TurnContext::default(),
        })
        .await
        .expect("first before_turn");
    assert!(first_before_turn.iter().any(|emitted| matches!(
        &emitted.value,
        PluginDirective::EnqueueMessages { messages }
            if messages.iter().any(|message| message.content.contains("Plan mode:"))
    )));

    session
        .after_turn(TurnResultHookContext {
            session_id: "root".to_string(),
            turn: Arc::new(lash_core::TurnResultSummary::from_assembled(&empty_turn(
                "root",
            ))),
            sessions: sessions(&manager),
        })
        .await
        .expect("after_turn");

    let second_before_turn = session
        .before_turn(TurnHookContext {
            session_id: "root".to_string(),
            state: mock_read_view("run-session"),
            sessions: sessions(&manager),
            turn_context: lash_core::TurnContext::default(),
        })
        .await
        .expect("second before_turn");
    assert!(second_before_turn.is_empty());
}

#[tokio::test]
async fn plan_mode_plugin_uses_configured_allowlist() {
    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let host = plan_mode_host(PlanModePluginFactory::new(
        PlanModePluginConfig::default().with_allowed_tools(["edit", "read_file"]),
    ));
    let session = host.build_session("root", None).expect("session");
    let manager = Arc::new(mock_session_manager("run-session"));

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    let allowed = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "read_file".to_string(),
            json!({"path":"src/main.rs"}),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(allowed.is_empty());

    let edit_allowed = session
        .before_tool_call(ToolCallHookContext::new(
            "root".to_string(),
            "edit".to_string(),
            json!({
                "path": ".lash/plans/run-session.md",
                "edits": [{ "oldText": "TBD", "newText": "Plan" }]
            }),
            lash_core::ToolArgumentProjectionPolicy::default(),
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("before_tool_call");
    assert!(edit_allowed.is_empty());
}

#[tokio::test]
async fn plan_mode_tool_exit_disables_mode_after_user_approval() {
    struct PromptingSessionManager {
        base: MockSessionManager,
    }
    impl_plan_test_host!(PromptingSessionManager);

    #[async_trait::async_trait]
    impl PlanTestHostCore for PromptingSessionManager {
        async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
            self.base.snapshot_current().await
        }

        async fn snapshot_session(&self, session_id: &str) -> Result<SessionSnapshot, PluginError> {
            self.base.snapshot_session(session_id).await
        }

        async fn tool_catalog(
            &self,
            session_id: &str,
        ) -> Result<Vec<serde_json::Value>, PluginError> {
            self.base.tool_catalog(session_id).await
        }

        async fn tool_state(&self, session_id: &str) -> Result<lash_core::ToolState, PluginError> {
            self.base.tool_state(session_id).await
        }

        async fn apply_tool_state(
            &self,
            session_id: &str,
            snapshot: lash_core::ToolState,
        ) -> Result<u64, PluginError> {
            self.base.apply_tool_state(session_id, snapshot).await
        }

        async fn create_session(
            &self,
            request: SessionCreateRequest,
        ) -> Result<SessionHandle, PluginError> {
            self.base.create_session(request).await
        }

        async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
            self.base.close_session(session_id).await
        }

        async fn prompt_user(
            &self,
            request: lash_plugin_plan_mode::PlanModePromptRequest,
        ) -> Result<lash_plugin_plan_mode::PlanModePromptResponse, PluginError> {
            assert!(request.question.contains(".lash/plans/run-session.md"));
            assert!(request.allow_note);
            let review = request.review.expect("plan review");
            assert_eq!(review.title, "PLAN");
            assert_eq!(review.markdown, ready_plan_markdown().trim_end());
            Ok(lash_plugin_plan_mode::PlanModePromptResponse::Single {
                selection: "Start implementing now".to_string(),
                note: Some("start with the safe slice".to_string()),
            })
        }
    }

    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    std::fs::create_dir_all(
        plan_file_path(temp.path(), "run-session")
            .parent()
            .expect("parent"),
    )
    .expect("plan dir");
    std::fs::write(
        plan_file_path(temp.path(), "run-session"),
        ready_plan_markdown(),
    )
    .expect("write plan");

    let manager = Arc::new(PromptingSessionManager {
        base: mock_session_manager("run-session"),
    });
    let manager_host = manager.clone();
    let host = plan_mode_host(
        PlanModePluginFactory::new(PlanModePluginConfig::default()).with_prompt(manager.clone()),
    );
    let session = host.build_session("root", None).expect("session");

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager_host).await;
    session
        .before_turn(TurnHookContext {
            session_id: "root".to_string(),
            state: mock_read_view("run-session"),
            sessions: sessions(&manager_host),
            turn_context: lash_core::TurnContext::default(),
        })
        .await
        .expect("before_turn");

    showcase_plan_exit_for_direct_execute(&session);
    let plan_exit_args = json!({});
    let plan_exit_ctx = lash_core::testing::mock_tool_context_with_host(manager_host.clone());
    let result = session
        .tools()
        .execute(lash_core::ToolCall {
            name: "plan_exit",
            args: &plan_exit_args,
            context: &plan_exit_ctx,
            progress: None,
        })
        .await;
    assert!(result.is_success(), "{:?}", result.value_for_projection());
    let result_value = result.value_for_projection();
    assert_eq!(
        result_value
            .get("approved")
            .and_then(|value| value.as_bool()),
        Some(true)
    );
    assert!(
        result_value
            .get("next_turn_input")
            .and_then(|value| value.as_str())
            .is_some_and(|value| {
                value.contains(
                    "The user approved the plan. Execute the plan in `.lash/plans/run-session.md` now"
                )
                    && value.contains("User note: start with the safe slice")
            })
    );

    // Executing plan_exit disables plan mode, so the catalog contribution
    // removes plan_exit from the catalog (non-membership).
    assert!(
        !session
            .resolved_tool_catalog("root")
            .expect("catalog")
            .has_callable_tool("plan_exit")
    );
}

#[tokio::test]
async fn plan_mode_tool_exit_allows_exit_without_validation() {
    struct PromptingSessionManager;
    impl_plan_test_host!(PromptingSessionManager);

    #[async_trait::async_trait]
    impl PlanTestHostCore for PromptingSessionManager {
        async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
            Ok(mock_snapshot("run-session"))
        }

        async fn snapshot_session(
            &self,
            _session_id: &str,
        ) -> Result<SessionSnapshot, PluginError> {
            Ok(mock_snapshot("run-session"))
        }

        async fn tool_catalog(
            &self,
            _session_id: &str,
        ) -> Result<Vec<serde_json::Value>, PluginError> {
            Ok(Vec::new())
        }

        async fn create_session(
            &self,
            request: SessionCreateRequest,
        ) -> Result<SessionHandle, PluginError> {
            let base = mock_session_manager("run-session");
            base.create_session(request).await
        }

        async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
            let base = mock_session_manager("run-session");
            base.close_session(session_id).await
        }

        async fn prompt_user(
            &self,
            request: lash_plugin_plan_mode::PlanModePromptRequest,
        ) -> Result<lash_plugin_plan_mode::PlanModePromptResponse, PluginError> {
            assert_eq!(
                request.question,
                "Review the plan in `.lash/plans/run-session.md`. What next?"
            );
            let review = request.review.expect("plan review");
            assert_eq!(review.title, "PLAN");
            assert!(review.markdown.contains("# Plan"));
            Ok(lash_plugin_plan_mode::PlanModePromptResponse::Single {
                selection: "Start implementing now".to_string(),
                note: None,
            })
        }
    }

    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let manager = Arc::new(PromptingSessionManager);
    let host = plan_mode_host(PlanModePluginFactory::default().with_prompt(manager.clone()));
    let session = host.build_session("root", None).expect("session");

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    session
        .before_turn(TurnHookContext {
            session_id: "root".to_string(),
            state: mock_read_view("run-session"),
            sessions: sessions(&manager),
            turn_context: lash_core::TurnContext::default(),
        })
        .await
        .expect("before_turn");

    showcase_plan_exit_for_direct_execute(&session);
    let plan_exit_args = json!({});
    let plan_exit_ctx = lash_core::testing::mock_tool_context_with_host(manager.clone());
    let result = session
        .tools()
        .execute(lash_core::ToolCall {
            name: "plan_exit",
            args: &plan_exit_args,
            context: &plan_exit_ctx,
            progress: None,
        })
        .await;
    assert!(result.is_success(), "{:?}", result.value_for_projection());
    let result_value = result.value_for_projection();
    assert!(
        result_value
            .get("approved")
            .and_then(|value| value.as_bool())
            == Some(true)
    );
    assert_eq!(
        result_value
            .get("execution_mode")
            .and_then(|value| value.as_str()),
        Some("current_session")
    );
}

#[tokio::test]
async fn plan_mode_tool_exit_can_execute_with_fresh_context() {
    struct PromptingSessionManager;
    impl_plan_test_host!(PromptingSessionManager);

    #[async_trait::async_trait]
    impl PlanTestHostCore for PromptingSessionManager {
        async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
            Ok(mock_snapshot("run-session"))
        }

        async fn snapshot_session(
            &self,
            _session_id: &str,
        ) -> Result<SessionSnapshot, PluginError> {
            Ok(mock_snapshot("run-session"))
        }

        async fn tool_catalog(
            &self,
            _session_id: &str,
        ) -> Result<Vec<serde_json::Value>, PluginError> {
            Ok(Vec::new())
        }

        async fn create_session(
            &self,
            request: SessionCreateRequest,
        ) -> Result<SessionHandle, PluginError> {
            let base = mock_session_manager("run-session");
            base.create_session(request).await
        }

        async fn close_session(&self, session_id: &str) -> Result<(), PluginError> {
            let base = mock_session_manager("run-session");
            base.close_session(session_id).await
        }

        async fn prompt_user(
            &self,
            _request: lash_plugin_plan_mode::PlanModePromptRequest,
        ) -> Result<lash_plugin_plan_mode::PlanModePromptResponse, PluginError> {
            Ok(lash_plugin_plan_mode::PlanModePromptResponse::Single {
                selection: "Start in fresh context".to_string(),
                note: None,
            })
        }
    }

    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let manager = Arc::new(PromptingSessionManager);
    let host = plan_mode_host(PlanModePluginFactory::default().with_prompt(manager.clone()));
    let session = host.build_session("root", None).expect("session");

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    session
        .before_turn(TurnHookContext {
            session_id: "root".to_string(),
            state: mock_read_view("run-session"),
            sessions: sessions(&manager),
            turn_context: lash_core::TurnContext::default(),
        })
        .await
        .expect("before_turn");

    showcase_plan_exit_for_direct_execute(&session);
    let plan_exit_args = json!({});
    let plan_exit_ctx = lash_core::testing::mock_tool_context_with_host(manager.clone());
    let result = session
        .tools()
        .execute(lash_core::ToolCall {
            name: "plan_exit",
            args: &plan_exit_args,
            context: &plan_exit_ctx,
            progress: None,
        })
        .await;
    assert!(result.is_success(), "{:?}", result.value_for_projection());
    let result_value = result.value_for_projection();
    assert_eq!(
        result_value
            .get("execution_mode")
            .and_then(|value| value.as_str()),
        Some("fresh_context")
    );
    assert!(
        result_value.get("fresh_context_input").is_none(),
        "fresh-context execution is now carried by the frame-switch control, not the tool result"
    );
}

#[tokio::test]
async fn plan_mode_after_tool_call_switches_agent_frame_on_fresh_context_approval() {
    #[derive(Clone, Default)]
    struct CapturingSessionManager {
        created: Arc<std::sync::Mutex<Vec<SessionCreateRequest>>>,
    }
    impl_plan_test_host!(CapturingSessionManager);

    #[async_trait::async_trait]
    impl PlanTestHostCore for CapturingSessionManager {
        async fn snapshot_current(&self) -> Result<SessionSnapshot, PluginError> {
            Ok(mock_snapshot("run-session"))
        }

        async fn snapshot_session(
            &self,
            _session_id: &str,
        ) -> Result<SessionSnapshot, PluginError> {
            Ok(mock_snapshot("run-session"))
        }

        async fn tool_catalog(
            &self,
            _session_id: &str,
        ) -> Result<Vec<serde_json::Value>, PluginError> {
            Ok(Vec::new())
        }

        async fn create_session(
            &self,
            request: SessionCreateRequest,
        ) -> Result<SessionHandle, PluginError> {
            self.created.lock().expect("created").push(request.clone());
            Ok(SessionHandle {
                session_id: request
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "new-session".to_string()),
                parent_session_id: request.relation.parent_session_id().map(ToOwned::to_owned),
                policy: request.policy.clone().unwrap_or_default(),
            })
        }

        async fn close_session(&self, _session_id: &str) -> Result<(), PluginError> {
            Ok(())
        }
    }

    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let host = plan_mode_host(PlanModePluginFactory::default());
    let session = host.build_session("root", None).expect("session");
    let manager = Arc::new(CapturingSessionManager::default());

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    let directives = session
        .after_tool_call(ToolResultHookContext::new(
            "root".to_string(),
            "plan_exit".to_string(),
            json!({}),
            ToolResult::ok(json!({
                "approved": true,
                "plan_path": ".lash/plans/run-session.md",
                "execution_mode": "fresh_context",
            })),
            1,
            lash_core::TurnContext::default(),
            manager.clone(),
        ))
        .await
        .expect("after_tool_call");

    assert_eq!(manager.created.lock().expect("created").len(), 0);
    let switch = directives
        .iter()
        .find_map(|owned| match &owned.value {
            PluginDirective::ShortCircuitTool { output } => output.control.as_ref(),
            _ => None,
        })
        .expect("short circuit carries frame switch");
    match switch {
        lash_core::ToolControl::SwitchAgentFrame { frame_id, task, .. } => {
            assert!(frame_id.starts_with("root:frame:plan-exit:after:"));
            assert_eq!(
                task.as_deref(),
                Some(
                    "Do a full, faithful implementation of the plan found at: .lash/plans/run-session.md"
                )
            );
        }
        other => panic!("expected frame switch, got {other:?}"),
    }
    assert!(
        directives
            .iter()
            .any(|owned| matches!(owned.value, PluginDirective::EmitRuntimeEvents { .. }))
    );
}

#[tokio::test]
async fn plan_mode_plugin_does_not_rewrite_assistant_output() {
    let _guard = plan_mode_env_lock().lock().await;
    let temp = tempfile::tempdir().expect("tempdir");
    let _cwd = CurrentDirGuard::set(temp.path());
    let host = plan_mode_host(PlanModePluginFactory::default());
    let session = host.build_session("root", None).expect("session");
    let manager = Arc::new(mock_session_manager("run-session"));

    run_plan_command(&session, "plan_mode.enable", json!({}), &manager).await;

    let stream = session
        .transform_assistant_stream("root", "Keep this text exactly.".to_string())
        .await
        .expect("stream");
    assert!(stream.is_empty());

    let response = session
        .transform_assistant_response(
            "root",
            lash_core::llm::types::LlmResponse {
                full_text: "Keep this text exactly.".into(),
                parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                    text: "Keep this text exactly.".into(),
                    response_meta: None,
                }],
                usage: lash_core::llm::types::LlmUsage::default(),
                terminal_reason: lash_core::LlmTerminalReason::Stop,
                terminal_diagnostic: None,
                provider_usage: None,
                request_body: None,
                http_summary: None,
                execution_evidence: None,
                generation_disposition: None,
                response_metadata: Default::default(),
            },
        )
        .await
        .expect("response");
    assert!(response.is_empty());
}
