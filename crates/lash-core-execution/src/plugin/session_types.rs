pub use lash_core_store::session_identity::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, FrameNodeId, FrameNodeIdError,
    OpenAgentFrameOutcome, OpenAgentFrameRequest, SessionLineage, SessionObservedProcessOutcome,
    SessionObservedProcessReceipt, SessionObserverIntent, SessionRelation, SessionSnapshot,
    SessionStartPoint, SessionToolAccess, SessionToolAccessError,
};

use crate::SessionId;

use serde::{Deserialize, Serialize};

use super::*;
use crate::SessionAppendNode;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionHandle {
    pub session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<SessionId>,
    pub policy: SessionPolicy,
    /// Per-id outcome for observer edges requested at session creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_processes: Vec<SessionObservedProcessReceipt>,
}

#[derive(Clone, Debug)]
pub struct PluginOwned<T> {
    pub plugin_id: String,
    pub value: T,
}

#[cfg(test)]
mod frame_node_id_tests {
    use super::{FrameNodeId, FrameNodeIdError};

    #[test]
    fn frame_node_id_rejects_empty_api_and_serialized_values() {
        assert_eq!(FrameNodeId::new(""), Err(FrameNodeIdError::Empty));
        assert!(
            serde_json::from_str::<FrameNodeId>(r#""""#)
                .expect_err("empty serialized frame node id must be rejected")
                .to_string()
                .contains("frame node id must not be empty")
        );
    }
}

/// The part of its session's config a host session-turn start left
/// unstated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnstatedSessionConfig {
    /// The request carries no policy: no turn budget, generation or charge
    /// safety is stated.
    Policy,
    /// The request names no model: neither a key nor a recorded binding.
    Model,
}

impl std::fmt::Display for UnstatedSessionConfig {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Policy => "policy",
            Self::Model => "model",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub relation: SessionRelation,
    pub start: SessionStartPoint,
    #[serde(default)]
    pub policy: Option<SessionPolicy>,
    /// The initial prompt plan chosen by the creator. An unstated plan uses
    /// the neutral default, regardless of the session relation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_plan: Option<crate::prompt_sections::PromptPlan>,
    /// A model key the child runs instead of the recorded model its policy
    /// carries. The creating runtime's models mint it when the child is
    /// created, and the child records that binding with its config. `None`
    /// keeps the policy's recorded model verbatim; nothing re-resolves it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<crate::LlmProfileKey>,
    /// The reasoning the child runs the model its key mints with. `None`
    /// keeps the reasoning its policy's recorded model carries, or the
    /// provider's default when the policy records no model. Stated beside
    /// the key because a policy carries reasoning only with a minted model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<crate::ReasoningSelection>,
    #[serde(default)]
    pub initial_nodes: Vec<SessionAppendNode>,
    /// Host-selected process observer edges to apply after session creation.
    ///
    /// Edge application is idempotent, durably recoverable when the session
    /// has a store, and reported per process on the returned [`SessionHandle`].
    /// Unknown or pruned ids do not fail session creation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed_processes: Vec<crate::ProcessId>,
    pub tool_access: SessionToolAccess,
    /// Plugin-owned options that configure plugin behavior at session
    /// creation time. Each plugin decodes only the entry keyed by its id.
    #[serde(default)]
    pub plugin_options: PluginOptions,
}

impl SessionCreateRequest {
    pub fn root(start: SessionStartPoint, plugin_options: PluginOptions) -> Self {
        Self {
            session_id: Some(SessionId::from_uuid(uuid::Uuid::new_v4().as_u128())),
            relation: SessionRelation::Root,
            start,
            policy: None,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            plugin_options,
            model: None,
            reasoning: None,
            prompt_plan: None,
        }
    }

    pub fn child_session(
        parent_session_id: impl Into<SessionId>,
        start: SessionStartPoint,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            session_id: Some(SessionId::from_uuid(uuid::Uuid::new_v4().as_u128())),
            relation: SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            policy: None,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            plugin_options,
            model: None,
            reasoning: None,
            prompt_plan: None,
        }
    }

    pub fn child(
        parent_session_id: impl Into<SessionId>,
        start: SessionStartPoint,
        policy: SessionPolicy,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            session_id: Some(SessionId::from_uuid(uuid::Uuid::new_v4().as_u128())),
            relation: SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            policy: Some(policy),
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            plugin_options,
            model: None,
            reasoning: None,
            prompt_plan: None,
        }
    }

    /// Start with exactly `plan`, without consulting any parent session.
    pub fn with_prompt_plan(mut self, plan: crate::prompt_sections::PromptPlan) -> Self {
        self.prompt_plan = Some(plan);
        self
    }

    /// Run the child on `key`, minted when the child is created.
    pub fn with_llm_profile(mut self, key: crate::LlmProfileKey) -> Self {
        self.model = Some(key);
        self
    }

    /// Whether the request names a model of its own: a key to mint, or a
    /// policy that records one.
    pub fn names_llm_profile(&self) -> bool {
        self.model.is_some()
            || self
                .policy
                .as_ref()
                .is_some_and(|policy| policy.model.is_some())
    }

    /// State `spec` as this request's whole config (FIG-4594): the policy
    /// fields it states, its model key and reasoning, and its plugin
    /// options. The key is carried unminted, so the request states nothing
    /// a catalog derives; the runtime that creates the session mints it. A
    /// host session-turn start states its session this way.
    ///
    /// # Errors
    ///
    /// A spec that states no model or no turn budget
    /// ([`SessionSpec::inherit`](crate::SessionSpec::inherit)).
    pub fn with_spec(
        mut self,
        spec: &crate::SessionSpec,
    ) -> Result<Self, crate::session_model::SpecResolveError> {
        let Some(model) = spec.model.clone() else {
            return Err(crate::session_model::SpecResolveError::RootWithoutLlmProfile);
        };
        self.policy = Some(spec.stated_root_policy()?);
        self.model = Some(model);
        self.reasoning = spec.reasoning.clone();
        self.plugin_options = spec.plugin_options.clone();
        Ok(self)
    }

    /// What a host start's request leaves unstated, if anything: a start no
    /// session captured an environment for has no base, so it states its
    /// policy and names its model (FIG-4594).
    pub fn unstated_root_config(&self) -> Option<UnstatedSessionConfig> {
        if self.policy.is_none() {
            Some(UnstatedSessionConfig::Policy)
        } else if !self.names_llm_profile() {
            Some(UnstatedSessionConfig::Model)
        } else {
            None
        }
    }

    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    pub fn with_initial_nodes(mut self, initial_nodes: Vec<SessionAppendNode>) -> Self {
        self.initial_nodes = initial_nodes;
        self
    }

    /// Sets the observed processes carried by a `SessionCreateRequest` for store and process-engine
    /// implementors while persisting and coordinating durable process execution.
    pub fn with_observed_processes(
        mut self,
        process_ids: impl IntoIterator<Item = impl Into<crate::ProcessId>>,
    ) -> Self {
        self.observed_processes = process_ids.into_iter().map(Into::into).collect();
        self
    }

    pub fn with_tool_access(mut self, tool_access: SessionToolAccess) -> Self {
        self.tool_access = tool_access;
        self
    }

    pub fn with_caused_by(mut self, caused_by: crate::CausalRef) -> Self {
        if let SessionRelation::Child {
            caused_by: cause, ..
        } = &mut self.relation
        {
            *cause = Some(caused_by);
        }
        self
    }
}

#[cfg(test)]
mod session_tool_access_tests {
    use super::{SessionToolAccess, SessionToolAccessError};

    fn tool(id: &str, name: &str) -> crate::ToolDefinition {
        crate::ToolDefinition::raw(
            id,
            name,
            format!("{name} description"),
            crate::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "string" }),
        )
        .expect("valid declared tool schemas")
        .with_execution(std::time::Duration::from_secs(120))
    }

    #[test]
    fn ambient_and_restricted_empty_have_distinct_canonical_encodings() {
        assert_eq!(
            serde_json::to_value(SessionToolAccess::ambient()).expect("serialize ambient"),
            serde_json::json!({ "mode": "ambient" })
        );
        let restricted = SessionToolAccess::restricted([]).expect("empty restriction is valid");
        assert_eq!(
            serde_json::to_value(&restricted).expect("serialize restricted empty"),
            serde_json::json!({ "mode": "restricted", "tools": [] })
        );
        assert_eq!(
            restricted
                .restricted_tools()
                .expect("explicit restricted mode")
                .len(),
            0
        );
    }

    #[test]
    fn checked_construction_rejects_invalid_restricted_definitions() {
        let empty_name = SessionToolAccess::restricted([tool("tool:empty", " ")])
            .expect_err("blank name must refuse");
        assert_eq!(
            empty_name,
            SessionToolAccessError::EmptyToolName { index: 0 }
        );

        let duplicate_name = SessionToolAccess::restricted([
            tool("tool:first", "duplicate"),
            tool("tool:second", "duplicate"),
        ])
        .expect_err("duplicate name must refuse");
        assert_eq!(
            duplicate_name,
            SessionToolAccessError::DuplicateToolName {
                name: "duplicate".to_string()
            }
        );

        let duplicate_id = SessionToolAccess::restricted([
            tool("tool:duplicate", "first"),
            tool("tool:duplicate", "second"),
        ])
        .expect_err("duplicate id must refuse");
        assert!(matches!(
            duplicate_id,
            SessionToolAccessError::DuplicateToolId { tool_id }
                if tool_id.as_str() == "tool:duplicate"
        ));

        assert_eq!(
            SessionToolAccess::ambient()
                .with_hidden_tools([" "])
                .expect_err("blank hidden name must refuse"),
            SessionToolAccessError::EmptyHiddenToolName { index: 0 }
        );
        assert_eq!(
            SessionToolAccess::ambient()
                .with_hidden_tools(["duplicate", "duplicate"])
                .expect_err("duplicate hidden name must refuse"),
            SessionToolAccessError::DuplicateHiddenToolName {
                name: "duplicate".to_string()
            }
        );
    }
}

#[cfg(test)]
mod observer_intent_relation_cutover_tests {
    use super::SessionRelation;

    #[test]
    fn create_relation_rejects_empty_observer_intent_wrapper() {
        let error = serde_json::from_value::<SessionRelation>(serde_json::json!({
            "kind": "observer_intent",
            "relation": { "kind": "root" }
        }))
        .expect_err("an empty wrapper over root is no longer a relation");
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert!(
            error
                .to_string()
                .contains("unknown variant `observer_intent`")
        );
    }
}
