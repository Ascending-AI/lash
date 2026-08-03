//! `update_plan` tool + plugin.
//!
//! A root-only, interactive-only tool that lets the model publish a
//! checklist. Each call fully replaces the previously-published plan.
//! The plugin:
//!
//! * exposes one tool, `update_plan`, with status values `pending` /
//!   `in_progress` / `completed` (at most one `in_progress` at a time)
//! * stores the latest snapshot on the plugin so it survives resume /
//!   snapshot
//! * emits a semantic `update_plan.snapshot` runtime event after every
//!   successful call. CLI/TUI crates decide how to present that snapshot.
//!
//! Gating: the plugin's [`PluginFactory::build`] returns an inert
//! `SessionPlugin` whenever the session has a parent (i.e. the session
//! is a subagent, compaction child, or any other non-root session).
//! Interactive-vs-batch gating is host policy; the first-party implementation
//! lives in the external `lash-cli` Host Application.

use std::sync::{Arc, Mutex};

use serde_json::json;

use lash_core::plugin::{
    PluginDirective, PluginError, PluginFactory, PluginRegistrar, PluginSessionContext,
    SessionPlugin,
};
use lash_core::{PromptContribution, ToolCall, ToolDefinition, ToolResult};
use lash_tool_support::{
    LashlangToolBinding, StaticToolExecute, StaticToolProvider, ToolDefinitionLashlangExt,
};

const PLUGIN_ID: &str = "update_plan";
const UPDATE_PLAN_SNAPSHOT_EVENT: &str = "update_plan.snapshot";
const PLANNING_GUIDANCE: &str = concat!(
    "Use `plan.update` for substantial multi-step work and skip it for trivial or single-step asks. ",
    "Write short steps and keep exactly one step `in_progress` while work is underway. ",
    "Mark completed work before moving on, use `explanation` when the plan changes, and update the plan as soon as scope or sequencing shifts. ",
    "Do not let the plan go stale while coding or running validation. ",
    "After a `plan.update` call, briefly summarize what changed and what comes next instead of repeating the full checklist. ",
    "Finish by marking every step `completed` when the task is done.",
);

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanItem {
    pub step: String,
    pub status: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlanSnapshot {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plan: Vec<PlanItem>,
    #[serde(default)]
    pub generation: u64,
}

impl PlanSnapshot {
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Default)]
struct PlanState {
    explanation: Option<String>,
    items: Vec<PlanItem>,
    generation: u64,
}

impl PlanState {
    fn snapshot(&self) -> PlanSnapshot {
        PlanSnapshot {
            explanation: self.explanation.clone(),
            plan: self.items.clone(),
            generation: self.generation,
        }
    }

    fn apply(&mut self, explanation: Option<String>, items: Vec<PlanItem>) {
        self.explanation = explanation;
        self.items = items;
        self.generation = self.generation.wrapping_add(1).max(1);
    }
}

struct UpdatePlanTool {
    state: Arc<Mutex<PlanState>>,
}

fn update_plan_provider(state: Arc<Mutex<PlanState>>) -> StaticToolProvider<UpdatePlanTool> {
    StaticToolProvider::new(
        vec![update_plan_tool_definition()],
        UpdatePlanTool { state },
    )
}

#[async_trait::async_trait]
impl StaticToolExecute for UpdatePlanTool {
    async fn execute(&self, call: ToolCall<'_>) -> ToolResult {
        match call.name {
            "update_plan" => execute_update_plan(&self.state, call.args),
            other => ToolResult::err_fmt(format_args!("Unknown tool: {other}")),
        }
    }
}

fn update_plan_tool_definition() -> ToolDefinition {
    ToolDefinition::raw(
                "tool:update_plan",
                "update_plan",
                "Publish or replace the current plan: a list of short ordered steps with statuses (pending, in_progress, completed), plus an optional explanation. At most one step can be in_progress at a time. Each call fully replaces the previous plan. Use this for substantial multi-step work to keep progress visible to the user. After updating, briefly summarize what changed and what comes next instead of repeating the full checklist.",
                serde_json::json!({
                    "type": "object",
                    "properties": {
                        "explanation": { "type": "string" },
                        "plan": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "step": { "type": "string" },
                                    "status": {
                                        "type": "string",
                                        "enum": ["pending", "in_progress", "completed"]
                                    }
                                },
                                "required": ["step", "status"],
                                "additionalProperties": false
                            }
                        }
                    },
                    "required": ["plan"],
                    "additionalProperties": false
                }),
                serde_json::json!({ "type": "string" }),
            )
            .with_examples(vec![
                "{\"explanation\":\"I found the main renderer.\",\"plan\":[{\"step\":\"Inspect renderer\",\"status\":\"completed\"},{\"step\":\"Patch layout\",\"status\":\"in_progress\"},{\"step\":\"Run tests\",\"status\":\"pending\"}]}"
                    .into(),
            ])
            .with_lashlang_binding(LashlangToolBinding::new(["plan"], "update"))
}

fn execute_update_plan(state: &Arc<Mutex<PlanState>>, args: &serde_json::Value) -> ToolResult {
    let explanation = args
        .get("explanation")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let Some(raw_plan) = args.get("plan").and_then(|value| value.as_array()) else {
        return ToolResult::err_fmt("Missing required parameter: plan");
    };
    if raw_plan.is_empty() {
        return ToolResult::err_fmt("Plan must contain at least one step");
    }

    let mut items = Vec::with_capacity(raw_plan.len());
    for (idx, item) in raw_plan.iter().enumerate() {
        let Some(object) = item.as_object() else {
            return ToolResult::err_fmt(format_args!(
                "Invalid plan[{idx}]: expected object with step and status"
            ));
        };
        let Some(step) = object
            .get("step")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|value| !value.is_empty())
        else {
            return ToolResult::err_fmt(format_args!(
                "Invalid plan[{idx}].step: expected non-empty string"
            ));
        };
        let Some(status) = object
            .get("status")
            .and_then(|value| value.as_str())
            .map(str::trim)
        else {
            return ToolResult::err_fmt(format_args!(
                "Invalid plan[{idx}].status: expected string"
            ));
        };
        if !matches!(status, "pending" | "in_progress" | "completed") {
            return ToolResult::err_fmt(format_args!(
                "Invalid plan[{idx}].status: expected pending, in_progress, or completed"
            ));
        }
        items.push(PlanItem {
            step: step.to_string(),
            status: status.to_string(),
        });
    }

    let in_progress = items
        .iter()
        .filter(|item| item.status == "in_progress")
        .count();
    if in_progress > 1 {
        return ToolResult::err_fmt("Plan may contain at most one in_progress step");
    }

    let mut guard = state.lock().unwrap();
    guard.apply(explanation, items);
    ToolResult::ok(json!("Plan updated"))
}

fn plan_snapshot_event(
    snapshot: &PlanSnapshot,
) -> Result<lash_core::PluginRuntimeEvent, PluginError> {
    Ok(lash_core::PluginRuntimeEvent::Custom {
        name: UPDATE_PLAN_SNAPSHOT_EVENT.to_string(),
        payload: serde_json::to_value(snapshot).map_err(|err| {
            PluginError::Session(format!("failed to encode plan snapshot: {err}"))
        })?,
    })
}

fn planning_prompt_contributions() -> Vec<PromptContribution> {
    vec![PromptContribution::guidance("Planning", PLANNING_GUIDANCE)]
}

/// Public plugin factory. Host applications that want this plugin installed
/// push an instance onto their plugin factory list. In non-root sessions the
/// factory returns an inert plugin that registers nothing.
pub struct UpdatePlanPluginFactory;

impl UpdatePlanPluginFactory {
    pub fn new() -> Self {
        Self
    }
}

impl Default for UpdatePlanPluginFactory {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginFactory for UpdatePlanPluginFactory {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(UpdatePlanPlugin {
            active: ctx.is_root_session(),
            state: Arc::new(Mutex::new(PlanState::default())),
        }))
    }
}

struct UpdatePlanPlugin {
    active: bool,
    state: Arc<Mutex<PlanState>>,
}

impl SessionPlugin for UpdatePlanPlugin {
    fn id(&self) -> &'static str {
        PLUGIN_ID
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        if !self.active {
            return Ok(());
        }
        reg.prompt().contribute(Arc::new(|_ctx| {
            Box::pin(async move { Ok(planning_prompt_contributions()) })
        }));
        reg.tools()
            .provider(Arc::new(update_plan_provider(Arc::clone(&self.state))))?;
        let after_state = Arc::clone(&self.state);
        reg.tool_calls().after(Arc::new(move |ctx| {
            let state = Arc::clone(&after_state);
            Box::pin(async move {
                if ctx.tool_name != "update_plan" {
                    return Ok(Vec::new());
                }
                if !ctx.result.is_success() {
                    tracing::debug!(
                        target: "lash_core::update_plan",
                        "after_tool_call observed failed update_plan; skipping emit",
                    );
                    return Ok(Vec::new());
                }
                let snapshot = state
                    .lock()
                    .map_err(|_| PluginError::Session("update_plan state poisoned".to_string()))?
                    .snapshot();
                tracing::info!(
                    target: "lash_core::update_plan",
                    items = snapshot.plan.len(),
                    generation = snapshot.generation,
                    "emitting plan snapshot event",
                );
                Ok(vec![PluginDirective::emit_runtime_events(vec![
                    plan_snapshot_event(&snapshot)?,
                ])])
            })
        }));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::testing::{MockSessionManager, test_standard_protocol_factories};
    use lash_core::{
        PromptSlot, SessionReadView, SessionSnapshot, facade_support::PluginHost,
        facade_support::PromptHookContext,
    };

    #[tokio::test]
    async fn validates_shape() {
        let tool = update_plan_provider(Arc::new(Mutex::new(PlanState::default())));
        let result = lash_core::testing::run_tool(
            &tool,
            "update_plan",
            &json!({"plan":[{"step":"","status":"pending"}]}),
        )
        .await;
        assert!(!result.is_success());
    }

    #[tokio::test]
    async fn rejects_multiple_in_progress_steps() {
        let tool = update_plan_provider(Arc::new(Mutex::new(PlanState::default())));
        let result = lash_core::testing::run_tool(
            &tool,
            "update_plan",
            &json!({
                "plan":[
                    {"step":"a","status":"in_progress"},
                    {"step":"b","status":"in_progress"}
                ]
            }),
        )
        .await;
        assert!(!result.is_success());
    }

    #[tokio::test]
    async fn bumps_generation_on_success() {
        let state = Arc::new(Mutex::new(PlanState::default()));
        let tool = update_plan_provider(Arc::clone(&state));
        assert_eq!(state.lock().unwrap().generation, 0);
        let result = lash_core::testing::run_tool(
            &tool,
            "update_plan",
            &json!({
                "plan":[{"step":"one","status":"pending"}]
            }),
        )
        .await;
        assert!(result.is_success());
        assert_eq!(state.lock().unwrap().generation, 1);
    }

    #[test]
    fn plan_snapshot_event_encodes_snapshot() {
        let snapshot = PlanSnapshot {
            explanation: None,
            plan: vec![
                PlanItem {
                    step: "done work".into(),
                    status: "completed".into(),
                },
                PlanItem {
                    step: "current".into(),
                    status: "in_progress".into(),
                },
                PlanItem {
                    step: "later".into(),
                    status: "pending".into(),
                },
            ],
            generation: 1,
        };
        let event = plan_snapshot_event(&snapshot).expect("event");
        let lash_core::PluginRuntimeEvent::Custom { name, payload } = event else {
            panic!("expected custom event");
        };
        assert_eq!(name, UPDATE_PLAN_SNAPSHOT_EVENT);
        let decoded: PlanSnapshot = serde_json::from_value(payload).expect("snapshot payload");
        assert_eq!(decoded, snapshot);
    }

    #[test]
    fn factory_marks_child_sessions_inactive() {
        let factory = UpdatePlanPluginFactory::new();
        let root_ctx = PluginSessionContext {
            session_id: "root".into(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
            plugin_options: Default::default(),
            parent_session_id: None,
        };
        let child_ctx = PluginSessionContext {
            session_id: "child".into(),
            tool_access: lash_core::SessionToolAccess::default(),
            subagent: None,
            extensions: Default::default(),
            plugin_options: Default::default(),
            parent_session_id: Some("root".into()),
        };
        assert!(root_ctx.is_root_session());
        assert!(!child_ctx.is_root_session());
        factory.build(&root_ctx).expect("root build");
        factory.build(&child_ctx).expect("child build");
    }

    #[tokio::test]
    async fn root_session_contributes_planning_guidance() {
        let mut factories = test_standard_protocol_factories();
        factories.push(Arc::new(UpdatePlanPluginFactory::new()));
        let plugin_host = PluginHost::new(factories);
        let session = plugin_host.build_session("root", None).expect("session");

        let contributions = session
            .collect_prompt_contributions(PromptHookContext {
                session_id: "root".to_string(),
                sessions: Arc::new(MockSessionManager::default()),
                state: SessionReadView::from_snapshot(&SessionSnapshot::default()),
                protocol_turn_options: lash_core::ProtocolTurnOptions::default(),
                turn_context: lash_core::TurnContext::default(),
            })
            .await
            .expect("prompt contributions");

        let contribution = contributions
            .iter()
            .find(|contribution| contribution.title.as_deref() == Some("Planning"))
            .expect("planning guidance");
        assert_eq!(contribution.slot, PromptSlot::Guidance);
        assert_eq!(contribution.content.as_ref(), PLANNING_GUIDANCE);
    }

    #[tokio::test]
    async fn child_session_does_not_contribute_planning_guidance() {
        let mut factories = test_standard_protocol_factories();
        factories.push(Arc::new(UpdatePlanPluginFactory::new()));
        let plugin_host = PluginHost::new(factories);
        let session = plugin_host
            .build_session_with_parent(
                "child",
                Some("root".to_string()),
                None,
                lash_core::plugin::SessionAuthorityContext::default(),
            )
            .expect("session");

        let contributions = session
            .collect_prompt_contributions(PromptHookContext {
                session_id: "child".to_string(),
                sessions: Arc::new(MockSessionManager::default()),
                state: SessionReadView::from_snapshot(&SessionSnapshot::default()),
                protocol_turn_options: lash_core::ProtocolTurnOptions::default(),
                turn_context: lash_core::TurnContext::default(),
            })
            .await
            .expect("prompt contributions");

        assert!(
            !contributions
                .iter()
                .any(|contribution| contribution.title.as_deref() == Some("Planning"))
        );
    }
}
