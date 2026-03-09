use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex as StdMutex};

use serde_json::json;
use tokio::sync::{Notify, mpsc};
use tokio_util::sync::CancellationToken;

use crate::capabilities::{CapabilityId, tools_for_capability};
use crate::provider::AgentModels;
use crate::{
    Agent, AgentConfig, AgentEvent, DynamicStateSnapshot, Message, MessageRole, Part, PartKind,
    ProgressSender, PruneState, SandboxMessage, Session, ToolDefinition, ToolParam, ToolProvider,
    ToolResult,
};

use super::{CompositeTools, FilteredTools, require_str};

/// Intelligence tier determines model choice, capabilities, and turn limits.
enum Tier {
    Low,
    Medium,
    High,
}

impl Tier {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "low" => Some(Tier::Low),
            "medium" => Some(Tier::Medium),
            "high" => Some(Tier::High),
            _ => None,
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            Tier::Low => "low",
            Tier::Medium => "medium",
            Tier::High => "high",
        }
    }
}

fn pick_model_and_reasoning(
    config: &AgentConfig,
    models: &Option<AgentModels>,
    tier: &Tier,
) -> (String, Option<String>) {
    if let Some(m) = models {
        match tier {
            Tier::Low => {
                if let Some(ref q) = m.low {
                    let effort = config
                        .provider
                        .reasoning_effort_for_model(q)
                        .map(str::to_string);
                    return (q.clone(), effort);
                }
            }
            Tier::Medium => {
                if let Some(ref b) = m.medium {
                    let effort = config
                        .provider
                        .reasoning_effort_for_model(b)
                        .map(str::to_string);
                    return (b.clone(), effort);
                }
            }
            Tier::High => {
                if let Some(ref t) = m.high {
                    let effort = config
                        .provider
                        .reasoning_effort_for_model(t)
                        .map(str::to_string);
                    return (t.clone(), effort);
                }
            }
        }
    }

    if let Some((model, effort)) = config.provider.default_agent_model(tier.as_str()) {
        return (model.to_string(), effort.map(str::to_string));
    }

    let model = config.model.clone();
    let effort = config
        .provider
        .reasoning_effort_for_model(&model)
        .map(str::to_string)
        .or_else(|| config.reasoning_effort.clone());
    (model, effort)
}

/// A running sub-agent managed by AgentCall.
struct RunningAgent {
    /// Accumulated prose output from the agent (drainable).
    buffer: Arc<StdMutex<String>>,
    /// Final result once the agent completes.
    result: Arc<StdMutex<Option<serde_json::Value>>>,
    /// Notified when the agent finishes.
    done_notify: Arc<Notify>,
    /// Cancel token to kill the agent.
    cancel: CancellationToken,
}

/// Single agent-call tool that spawns sub-agents at different intelligence tiers.
/// Returns a handle immediately; use agent_result/agent_output/agent_kill to interact.
pub struct AgentCall {
    tools: Arc<dyn ToolProvider>,
    config: AgentConfig,
    agent_models: Option<AgentModels>,
    cancel: CancellationToken,
    agents: Arc<StdMutex<HashMap<String, RunningAgent>>>,
}

impl AgentCall {
    pub fn new(
        tools: Arc<dyn ToolProvider>,
        config: &AgentConfig,
        agent_models: Option<AgentModels>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            tools,
            config: config.clone(),
            agent_models,
            cancel,
            agents: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    fn build_agent_config(&self, tier: &Tier) -> AgentConfig {
        let (model, reasoning_effort) =
            pick_model_and_reasoning(&self.config, &self.agent_models, tier);
        let capabilities = if matches!(tier, Tier::Low) {
            self.config
                .capabilities
                .clone()
                .disable(CapabilityId::CoreWrite)
        } else {
            self.config.capabilities.clone()
        };
        let execution_mode = if matches!(tier, Tier::Low) {
            crate::ExecutionMode::NativeTools
        } else {
            self.config.execution_mode
        };
        AgentConfig {
            capabilities,
            model,
            reasoning_effort,
            session_id: self.config.session_id.clone(),
            provider: self.config.provider.clone(),
            sub_agent: true,
            include_soul: matches!(tier, Tier::High),
            max_turns: None,
            llm_log_path: self.config.llm_log_path.clone(),
            headless: self.config.headless,
            prompt_overrides: self.config.prompt_overrides.clone(),
            instruction_source: self.config.instruction_source.clone(),
            execution_mode,
            ..Default::default()
        }
    }

    fn low_tier_filtered_snapshot(
        &self,
        mut snapshot: DynamicStateSnapshot,
    ) -> DynamicStateSnapshot {
        // Default policy: no file mutation tools and no nested delegation in low tier.
        let denied: HashSet<&str> = [
            "write_file",
            "edit_file",
            "find_replace",
            "agent_call",
            "agent_result",
            "agent_output",
            "agent_kill",
        ]
        .into_iter()
        .collect();

        snapshot
            .tools
            .retain(|name, _| !denied.contains(name.as_str()));
        snapshot
            .profile
            .enabled_tools
            .retain(|name| !denied.contains(name.as_str()));
        snapshot.profile.enabled_capabilities.remove("core_write");
        snapshot.profile.enabled_capabilities.remove("delegation");
        for def in snapshot.capability_defs.values_mut() {
            def.tool_names
                .retain(|name| !denied.contains(name.as_str()));
        }
        snapshot
    }

    /// Build the toolset for a spawned sub-agent:
    /// - low: read-only base tools only (no nested delegation)
    /// - medium/high: base tools + agent_call for nested delegation
    fn session_tools_for_tier(&self, tier: &Tier) -> Arc<dyn ToolProvider> {
        if let Some(snapshot) = self.tools.dynamic_snapshot() {
            let snapshot = if matches!(tier, Tier::Low) {
                self.low_tier_filtered_snapshot(snapshot)
            } else {
                snapshot
            };
            if let Some(fork) = self.tools.fork_dynamic_with_snapshot(snapshot) {
                return fork;
            }
        }

        if matches!(tier, Tier::Low) {
            let write_tools: std::collections::HashSet<&str> =
                tools_for_capability(CapabilityId::CoreWrite)
                    .iter()
                    .copied()
                    .collect();
            let allowed: BTreeSet<String> = self
                .tools
                .definitions()
                .into_iter()
                .map(|d| d.name)
                .filter(|name| !write_tools.contains(name.as_str()))
                .filter(|name| {
                    !matches!(
                        name.as_str(),
                        "agent_call" | "agent_result" | "agent_output" | "agent_kill"
                    )
                })
                .collect();
            return Arc::new(FilteredTools::new(Arc::clone(&self.tools), allowed));
        }

        Arc::new(
            CompositeTools::new()
                .add_arc(Arc::clone(&self.tools))
                .add(AgentCall::new(
                    Arc::clone(&self.tools),
                    &self.config,
                    self.agent_models.clone(),
                    self.cancel.clone(),
                )),
        )
    }

    /// Spawn a sub-agent in the background and return a handle ID.
    async fn spawn_agent(&self, args: &serde_json::Value) -> ToolResult {
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        if prompt.is_empty() {
            return ToolResult::err(json!("Missing required parameter: prompt"));
        }

        let intelligence = args
            .get("intelligence")
            .and_then(|v| v.as_str())
            .unwrap_or_default();

        let tier = match Tier::from_str(intelligence) {
            Some(t) => t,
            None => {
                return ToolResult::err(json!(
                    "Missing or invalid 'intelligence' parameter: must be \"low\", \"medium\", or \"high\""
                ));
            }
        };

        let schema = args
            .get("schema")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        let agent_config = self.build_agent_config(&tier);
        let agent_execution_mode = agent_config.execution_mode;

        // Generate agent ID for the sub-agent
        let agent_id = uuid::Uuid::new_v4().to_string();
        let handle_id = uuid::Uuid::new_v4().to_string();

        // Create a new session with tier-specific tools.
        let session_tools = self.session_tools_for_tier(&tier);
        let mut session = match Session::new(
            Arc::clone(&session_tools),
            &agent_id,
            self.config.headless,
            agent_config.capabilities.clone(),
            agent_config.execution_mode,
        )
        .await
        {
            Ok(s) => s,
            Err(e) => {
                return ToolResult::err_fmt(format_args!(
                    "Failed to create sub-agent session: {e}"
                ));
            }
        };

        // Dynamic tool providers may carry capability IDs beyond the static enum.
        // Re-register capability payload so the child REPL helper surface reflects
        // the actual projected tool/capability set.
        if session.supports_repl()
            && let (Some(caps_json), Some(generation)) = (
                session_tools.dynamic_capabilities_payload_json(),
                session_tools.dynamic_generation(),
            )
            && let Err(e) = session.reconfigure(caps_json, generation).await
        {
            return ToolResult::err_fmt(format_args!(
                "Failed to reconfigure sub-agent session: {e}"
            ));
        }

        // Load parent memory/history into the child session via hidden state tools.
        let parent_history = args
            .get("_parent_history")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let parent_mem = args
            .get("_parent_mem")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if session.supports_repl() && (parent_history.is_some() || parent_mem.is_some()) {
            if let Some(ref hist_json) = parent_history
                && let Ok(turns) = serde_json::from_str::<serde_json::Value>(hist_json)
            {
                let _ = session
                    .tools()
                    .execute(
                        "history_load",
                        &json!({"__agent_id__": agent_id, "turns": turns}),
                    )
                    .await;
            }
            if let Some(ref mem_json) = parent_mem
                && let Ok(entries) = serde_json::from_str::<serde_json::Value>(mem_json)
            {
                let _ = session
                    .tools()
                    .execute(
                        "mem_load",
                        &json!({"__agent_id__": agent_id, "entries": entries}),
                    )
                    .await;
            }
        }

        let mut agent = Agent::new(session, agent_config, Some(agent_id));

        // Build the user message, optionally appending schema instructions
        let user_content = if let Some(ref schema_str) = schema {
            let model_name = serde_json::from_str::<serde_json::Value>(schema_str)
                .ok()
                .and_then(|v| v.get("title").and_then(|t| t.as_str()).map(String::from))
                .unwrap_or_else(|| "Result".to_string());
            match agent_execution_mode {
                crate::ExecutionMode::Repl => format!(
                    "{prompt}\n\nCall done(...) with a single JSON-compatible dict matching this schema exactly:\n{schema_str}\n\nDo not return prose in done(). The dict should represent a `{model_name}` object."
                ),
                crate::ExecutionMode::NativeTools => format!(
                    "{prompt}\n\nReturn your final answer as a single JSON object matching this schema exactly:\n{schema_str}\n\nDo not wrap it in markdown fences or extra commentary."
                ),
            }
        } else {
            prompt.to_string()
        };

        let messages = vec![Message {
            id: uuid::Uuid::new_v4().to_string(),
            role: MessageRole::User,
            parts: vec![Part {
                id: "p0".to_string(),
                kind: PartKind::Text,
                content: user_content,
                tool_call_id: None,
                tool_name: None,
                prune_state: PruneState::Intact,
            }],
        }];

        let (event_tx, mut event_rx) = mpsc::channel::<AgentEvent>(100);
        let agent_cancel = self.cancel.child_token();
        let kill_token = agent_cancel.clone();

        // Set up shared state
        let buffer = Arc::new(StdMutex::new(String::new()));
        let context = Arc::new(StdMutex::new(Vec::new()));
        let result: Arc<StdMutex<Option<serde_json::Value>>> = Arc::new(StdMutex::new(None));
        let done_notify = Arc::new(Notify::new());

        let buf_clone = buffer.clone();
        let ctx_clone = context.clone();
        let res_clone = result.clone();
        let done_clone = done_notify.clone();

        // Spawn the agent run
        let run_handle =
            tokio::spawn(
                async move { agent.run(messages, vec![], event_tx, agent_cancel, 0).await },
            );

        let prompt_for_meta = prompt.to_string();

        // Spawn event drainer
        tokio::spawn(async move {
            let mut final_message: Option<String> = None;
            let mut current_prose = String::new();
            let mut cumulative_usage = crate::TokenUsage::default();
            let mut tool_call_count: usize = 0;

            while let Some(event) = event_rx.recv().await {
                match event {
                    AgentEvent::TextDelta { content } => {
                        current_prose.push_str(&content);
                        buf_clone.lock().unwrap().push_str(&content);
                    }
                    AgentEvent::Message { text, kind } => {
                        if kind == "final" {
                            final_message = Some(text);
                        }
                    }
                    AgentEvent::CodeBlock { .. } => {
                        let trimmed = current_prose.trim().to_string();
                        if !trimmed.is_empty() {
                            ctx_clone.lock().unwrap().push(trimmed);
                        }
                        current_prose.clear();
                    }
                    AgentEvent::TokenUsage { usage, .. } => {
                        cumulative_usage.add(&usage);
                    }
                    AgentEvent::ToolCall { .. } => {
                        tool_call_count += 1;
                    }
                    AgentEvent::Done => break,
                    _ => {}
                }
            }

            // Wait for the agent task to finish
            let (_, iterations) = run_handle.await.unwrap_or_default();

            let result_text = if let Some(msg) = final_message {
                msg
            } else if !current_prose.trim().is_empty() {
                current_prose.trim().to_string()
            } else {
                ctx_clone.lock().unwrap().join("\n\n")
            };

            let context_vec = ctx_clone.lock().unwrap().clone();
            let mut result_json = json!({"result": result_text, "context": context_vec});

            result_json["_sub_agent"] = json!({
                "task": prompt_for_meta,
                "usage": cumulative_usage,
                "tool_calls": tool_call_count,
                "iterations": iterations,
            });

            *res_clone.lock().unwrap() = Some(result_json);
            done_clone.notify_waiters();
        });

        // Store the running agent
        self.agents.lock().unwrap().insert(
            handle_id.clone(),
            RunningAgent {
                buffer,
                result,
                done_notify,
                cancel: kill_token,
            },
        );

        ToolResult::ok(json!({"__handle__": "agent", "id": handle_id}))
    }

    /// Wait for agent completion and return the result.
    async fn agent_result(
        &self,
        id: &str,
        timeout: Option<f64>,
        progress: Option<&ProgressSender>,
    ) -> ToolResult {
        let (result_arc, done_notify, buffer) = {
            let agents = self.agents.lock().unwrap();
            match agents.get(id) {
                Some(a) => (a.result.clone(), a.done_notify.clone(), a.buffer.clone()),
                None => return ToolResult::err_fmt(format_args!("No agent with id: {id}")),
            }
        };

        let deadline =
            timeout.map(|t| tokio::time::Instant::now() + std::time::Duration::from_secs_f64(t));

        let mut sent_len = 0usize;

        loop {
            let done = result_arc.lock().unwrap().is_some();

            // Stream new output to progress sender
            if let Some(tx) = progress {
                let buf = buffer.lock().unwrap();
                if buf.len() > sent_len {
                    let new_chunk = &buf[sent_len..];
                    let _ = tx.send(SandboxMessage {
                        text: new_chunk.to_string(),
                        kind: "tool_output".into(),
                    });
                    sent_len = buf.len();
                }
            }

            if done {
                break;
            }

            if let Some(dl) = deadline
                && tokio::time::Instant::now() >= dl
            {
                // Cancel the agent on timeout
                let cancel = {
                    let agents = self.agents.lock().unwrap();
                    agents.get(id).map(|a| a.cancel.clone())
                };
                if let Some(c) = cancel {
                    c.cancel();
                }
                self.agents.lock().unwrap().remove(id);
                return ToolResult::err(json!("Agent timed out"));
            }

            tokio::select! {
                _ = done_notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {}
            }
        }

        let result = result_arc.lock().unwrap().take().unwrap_or(json!(null));
        ToolResult::ok(result)
    }

    /// Drain accumulated output without waiting (non-blocking).
    fn agent_output(&self, id: &str) -> ToolResult {
        let agents = self.agents.lock().unwrap();
        match agents.get(id) {
            Some(a) => {
                let mut buf = a.buffer.lock().unwrap();
                let output = buf.clone();
                buf.clear();
                ToolResult::ok(json!(output))
            }
            None => ToolResult::err_fmt(format_args!("No agent with id: {id}")),
        }
    }

    /// Cancel a running agent.
    async fn agent_kill(&self, id: &str) -> ToolResult {
        let cancel = {
            let agents = self.agents.lock().unwrap();
            match agents.get(id) {
                Some(a) => a.cancel.clone(),
                None => return ToolResult::err_fmt(format_args!("No agent with id: {id}")),
            }
        };

        cancel.cancel();

        // Wait briefly for the agent to finish
        let done_notify = {
            let agents = self.agents.lock().unwrap();
            agents.get(id).map(|a| a.done_notify.clone())
        };

        if let Some(notify) = done_notify {
            let _ =
                tokio::time::timeout(std::time::Duration::from_secs(5), notify.notified()).await;
        }

        self.agents.lock().unwrap().remove(id);
        ToolResult::ok(json!(null))
    }
}

#[async_trait::async_trait]
impl ToolProvider for AgentCall {
    fn definitions(&self) -> Vec<ToolDefinition> {
        vec![
            ToolDefinition {
                name: "agent_call".into(),
                description: vec![
                    crate::ToolText::new(
                        r#"Spawn a sub-agent to perform a task. Returns a plain handle dict immediately; then use `await agent_result(handle["id"])`, `await agent_output(handle["id"])`, and `await agent_kill(handle["id"])`. Use intelligence="low" for fast read-only tasks (lookup/summarize); low-intelligence sub-agents always run in native-tools mode. Use medium/high for edits/refactors; those tiers inherit the parent session's execution mode. Sub-agents inherit your prior-turn history and memory context read-only."#,
                        [crate::ExecutionMode::Repl],
                    ),
                    crate::ToolText::new(
                        r#"Spawn a sub-agent to perform a task. Returns a plain handle dict. Use `agent_result(id)`, `agent_output(id)`, and `agent_kill(id)` with the returned handle ID. Use intelligence="low" for fast read-only tasks (lookup/summarize); low-intelligence sub-agents always run in native-tools mode. Use medium/high for edits/refactors; those tiers inherit the parent session's execution mode. Sub-agents inherit your prior-turn history and memory context read-only."#,
                        [crate::ExecutionMode::NativeTools],
                    ),
                ],
                params: vec![
                    ToolParam::typed("prompt", "str"),
                    ToolParam::typed("intelligence", "str"),
                    ToolParam {
                        name: "schema".into(),
                        r#type: "str".into(),
                        description: "JSON schema to include in the agent's prompt as output guidance (not enforced at runtime)".into(),
                        required: false,
                    },
                ],
                returns: "dict".into(),
                examples: vec![
                    crate::ToolText::new(
                        "handle = agent_call(\"Summarize the auth flow\", intelligence=\"low\")",
                        [crate::ExecutionMode::Repl],
                    ),
                    crate::ToolText::new(
                        "result = await agent_result(handle[\"id\"])",
                        [crate::ExecutionMode::Repl],
                    ),
                    crate::ToolText::new(
                        "text = await agent_output(handle[\"id\"])",
                        [crate::ExecutionMode::Repl],
                    ),
                    crate::ToolText::new(
                        "handle = agent_call(prompt=\"Summarize the auth flow\", intelligence=\"low\")",
                        [crate::ExecutionMode::NativeTools],
                    ),
                ],
                hidden: false,
                inject_into_prompt: true,
            },
            ToolDefinition {
                name: "agent_result".into(),
                description: vec![crate::ToolText::new(
                    "Wait for a sub-agent to finish and return its final result.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![
                    ToolParam::typed("id", "str"),
                    ToolParam::optional("timeout", "float"),
                ],
                returns: "dict".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "agent_output".into(),
                description: vec![crate::ToolText::new(
                    "Read and drain accumulated streaming output from a running sub-agent (non-blocking). Returns empty string if no new output. Agent must still be running — use before agent_result.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("id", "str")],
                returns: "str".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
            ToolDefinition {
                name: "agent_kill".into(),
                description: vec![crate::ToolText::new(
                    "Cancel a running sub-agent.",
                    [
                        crate::ExecutionMode::Repl,
                        crate::ExecutionMode::NativeTools,
                    ],
                )],
                params: vec![ToolParam::typed("id", "str")],
                returns: "None".into(),
                examples: vec![],
                hidden: true,
                inject_into_prompt: false,
            },
        ]
    }

    async fn execute(&self, name: &str, args: &serde_json::Value) -> ToolResult {
        match name {
            "agent_call" => self.spawn_agent(args).await,
            "agent_result" => {
                let id = match require_str(args, "id") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                let timeout = args.get("timeout").and_then(|v| v.as_f64());
                self.agent_result(id, timeout, None).await
            }
            "agent_output" => {
                let id = match require_str(args, "id") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                self.agent_output(id)
            }
            "agent_kill" => {
                let id = match require_str(args, "id") {
                    Ok(s) => s,
                    Err(e) => return e,
                };
                self.agent_kill(id).await
            }
            _ => ToolResult::err_fmt(format_args!("Unknown tool: {name}")),
        }
    }

    async fn execute_streaming(
        &self,
        name: &str,
        args: &serde_json::Value,
        progress: Option<&ProgressSender>,
    ) -> ToolResult {
        if name == "agent_result" {
            let id = match require_str(args, "id") {
                Ok(s) => s,
                Err(e) => return e,
            };
            let timeout = args.get("timeout").and_then(|v| v.as_f64());
            self.agent_result(id, timeout, progress).await
        } else {
            self.execute(name, args).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;

    fn codex_provider() -> Provider {
        Provider::Codex {
            access_token: "tok".into(),
            refresh_token: "ref".into(),
            expires_at: u64::MAX,
            account_id: Some("acct".into()),
        }
    }

    #[test]
    fn codex_uses_explicit_tier_models_when_no_overrides() {
        let config = AgentConfig {
            model: "custom-parent-model".to_string(),
            provider: codex_provider(),
            ..Default::default()
        };

        let (m_low, r_low) = pick_model_and_reasoning(&config, &None, &Tier::Low);
        assert_eq!(m_low, "gpt-5.3-codex-spark");
        assert_eq!(r_low, None);

        let (m_mid, r_mid) = pick_model_and_reasoning(&config, &None, &Tier::Medium);
        assert_eq!(m_mid, "gpt-5.4");
        assert_eq!(r_mid.as_deref(), Some("medium"));

        let (m_high, r_high) = pick_model_and_reasoning(&config, &None, &Tier::High);
        assert_eq!(m_high, "gpt-5.4");
        assert_eq!(r_high.as_deref(), Some("high"));
    }

    #[test]
    fn override_model_keeps_override_and_inferrs_reasoning() {
        let config = AgentConfig {
            model: "custom-parent-model".to_string(),
            provider: codex_provider(),
            ..Default::default()
        };
        let models = Some(AgentModels {
            low: None,
            medium: None,
            high: Some("gpt-5.4".to_string()),
        });

        let (m, r) = pick_model_and_reasoning(&config, &models, &Tier::High);
        assert_eq!(m, "gpt-5.4");
        assert_eq!(r.as_deref(), Some("high"));
    }

    #[test]
    fn agent_call_docs_are_mode_specific() {
        let defs = test_agent_call().definitions();
        let agent_call = defs
            .into_iter()
            .find(|def| def.name == "agent_call")
            .expect("agent_call definition");

        let repl_desc = agent_call.description_for(crate::ExecutionMode::Repl);
        let native_desc = agent_call.description_for(crate::ExecutionMode::NativeTools);

        assert!(repl_desc.contains("plain handle dict"));
        assert!(repl_desc.contains("await agent_result(handle[\"id\"])"));
        assert!(!native_desc.contains("AgentHandle"));
    }

    struct MockBaseTool;

    #[async_trait::async_trait]
    impl ToolProvider for MockBaseTool {
        fn definitions(&self) -> Vec<ToolDefinition> {
            vec![
                ToolDefinition {
                    name: "mock_base".into(),
                    description: vec![crate::ToolText::new(
                        "mock",
                        [
                            crate::ExecutionMode::Repl,
                            crate::ExecutionMode::NativeTools,
                        ],
                    )],
                    params: vec![],
                    returns: "None".into(),
                    examples: vec![],
                    hidden: false,
                    inject_into_prompt: true,
                },
                ToolDefinition {
                    name: "read_file".into(),
                    description: vec![crate::ToolText::new(
                        "mock read",
                        [
                            crate::ExecutionMode::Repl,
                            crate::ExecutionMode::NativeTools,
                        ],
                    )],
                    params: vec![],
                    returns: "str".into(),
                    examples: vec![],
                    hidden: false,
                    inject_into_prompt: true,
                },
                ToolDefinition {
                    name: "write_file".into(),
                    description: vec![crate::ToolText::new(
                        "mock write",
                        [
                            crate::ExecutionMode::Repl,
                            crate::ExecutionMode::NativeTools,
                        ],
                    )],
                    params: vec![],
                    returns: "None".into(),
                    examples: vec![],
                    hidden: false,
                    inject_into_prompt: true,
                },
            ]
        }

        async fn execute(&self, _name: &str, _args: &serde_json::Value) -> ToolResult {
            ToolResult::ok(serde_json::json!(null))
        }
    }

    fn test_agent_call() -> AgentCall {
        let config = AgentConfig {
            model: "custom-parent-model".to_string(),
            provider: codex_provider(),
            ..Default::default()
        };
        AgentCall::new(
            Arc::new(MockBaseTool),
            &config,
            None,
            CancellationToken::new(),
        )
    }

    #[test]
    fn low_tier_subagent_tools_do_not_include_agent_call() {
        let agent_call = test_agent_call();
        let tools = agent_call.session_tools_for_tier(&Tier::Low);
        let tool_names: Vec<String> = tools.definitions().into_iter().map(|d| d.name).collect();
        assert!(!tool_names.iter().any(|n| n == "agent_call"));
        assert!(tool_names.iter().any(|n| n == "mock_base"));
        assert!(tool_names.iter().any(|n| n == "read_file"));
        assert!(!tool_names.iter().any(|n| n == "write_file"));
    }

    #[test]
    fn medium_and_high_tier_subagent_tools_include_agent_call() {
        let agent_call = test_agent_call();

        let medium_tools = agent_call.session_tools_for_tier(&Tier::Medium);
        let medium_names: Vec<String> = medium_tools
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(medium_names.iter().any(|n| n == "agent_call"));
        assert!(medium_names.iter().any(|n| n == "mock_base"));
        assert!(medium_names.iter().any(|n| n == "write_file"));

        let high_tools = agent_call.session_tools_for_tier(&Tier::High);
        let high_names: Vec<String> = high_tools
            .definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(high_names.iter().any(|n| n == "agent_call"));
        assert!(high_names.iter().any(|n| n == "mock_base"));
        assert!(high_names.iter().any(|n| n == "write_file"));
    }

    #[test]
    fn low_tier_subagent_config_is_read_only() {
        let agent_call = test_agent_call();
        let low = agent_call.build_agent_config(&Tier::Low);
        let medium = agent_call.build_agent_config(&Tier::Medium);
        assert!(!low.capabilities.enabled(CapabilityId::CoreWrite));
        assert!(medium.capabilities.enabled(CapabilityId::CoreWrite));
        assert_eq!(low.execution_mode, crate::ExecutionMode::NativeTools);
        assert_eq!(medium.execution_mode, crate::ExecutionMode::Repl);
    }
}
