pub use lash_core_store::session_identity::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, FrameNodeId, FrameNodeIdError,
    OpenAgentFrameRequest, OpenAgentFrameResult, SessionLineage, SessionObservedProcessOutcome,
    SessionObservedProcessReceipt, SessionObserverIntent, SessionObserverIntentAttribution,
    SessionRelation, SessionSnapshot, SessionStartPoint, SessionToolAccess, SessionToolAccessError,
    SubagentSessionContext,
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPluginSource {
    CurrentHostFresh,
    #[default]
    CurrentSessionFork,
}

#[cfg(test)]
mod agent_frame_reason_tests {
    use super::AgentFrameReason;

    #[test]
    fn agent_frame_reason_round_trips_arbitrary_labels() {
        let reason: AgentFrameReason =
            serde_json::from_str("\"plan_mode\"").expect("deserialize reason");

        assert_eq!(reason.as_str(), "plan_mode");
        assert_eq!(
            serde_json::to_string(&reason).expect("serialize reason"),
            "\"plan_mode\""
        );
        assert_eq!(AgentFrameReason::compaction().as_str(), "compaction");
    }
}

#[cfg(test)]
mod frame_node_id_tests {
    use super::{FrameNodeId, FrameNodeIdError};

    #[test]
    fn frame_node_id_round_trips_non_empty_identity() {
        let encoded = r#""frame-node/v3/derived""#;
        let frame_node_id: FrameNodeId =
            serde_json::from_str(encoded).expect("deserialize frame node id");

        assert_eq!(
            serde_json::to_string(&frame_node_id).expect("serialize frame node id"),
            encoded
        );
    }

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

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionCreateRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default)]
    pub relation: SessionRelation,
    pub start: SessionStartPoint,
    #[serde(default)]
    pub policy: Option<SessionPolicy>,
    #[serde(default)]
    pub plugin_source: SessionPluginSource,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentSessionContext>,
    #[serde(skip)]
    pub context_overlay: SessionContextOverlay,
    /// Plugin-owned options that configure plugin behavior at session
    /// creation time. Each plugin decodes only the entry keyed by its id.
    #[serde(default)]
    pub plugin_options: PluginOptions,
    /// Label for the token-cost ledger. When this session's turns
    /// complete, their token usage is accumulated under this label on
    /// the parent session's `token_ledger`. Examples: `"subagent"`,
    /// `"compaction"`. Defaults to `"child"` if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage_source: Option<String>,
}

impl SessionCreateRequest {
    /// Builds a root-session request with a fresh UUID for protocol implementors materializing a
    /// new independent session.
    pub fn root(start: SessionStartPoint, plugin_options: PluginOptions) -> Self {
        Self {
            session_id: Some(SessionId::from(uuid::Uuid::new_v4().to_string())),
            relation: SessionRelation::Root,
            start,
            policy: None,
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: None,
        }
    }

    /// Builds a child-session request with a fresh UUID and inherited policy selection for protocol
    /// implementors materializing nested work.
    pub fn child_session(
        parent_session_id: impl Into<SessionId>,
        start: SessionStartPoint,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            session_id: Some(SessionId::from(uuid::Uuid::new_v4().to_string())),
            relation: SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            policy: None,
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: None,
        }
    }

    /// Builds a child-session request with an explicit policy and usage-ledger source for protocol
    /// and process-engine implementors materializing nested work.
    pub fn child(
        parent_session_id: impl Into<SessionId>,
        start: SessionStartPoint,
        policy: SessionPolicy,
        plugin_options: PluginOptions,
        usage_source: impl Into<String>,
    ) -> Self {
        Self::related(
            SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            Some(policy),
            plugin_options,
            usage_source,
        )
    }

    fn related(
        relation: SessionRelation,
        start: SessionStartPoint,
        policy: Option<SessionPolicy>,
        plugin_options: PluginOptions,
        usage_source: impl Into<String>,
    ) -> Self {
        Self {
            session_id: Some(SessionId::from(uuid::Uuid::new_v4().to_string())),
            relation,
            start,
            policy,
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            context_overlay: SessionContextOverlay::default(),
            plugin_options,
            usage_source: Some(usage_source.into()),
        }
    }

    /// Sets the plugin source carried by a `SessionCreateRequest` for protocol and process-engine
    /// implementors while preparing or executing plugin and tool work.
    pub fn with_plugin_source(mut self, plugin_source: SessionPluginSource) -> Self {
        self.plugin_source = plugin_source;
        self
    }

    /// Sets the session id carried by a `SessionCreateRequest` for store, effect-host, and protocol
    /// implementors while materializing, executing, or persisting a session turn.
    pub fn with_session_id(mut self, session_id: impl Into<SessionId>) -> Self {
        self.session_id = Some(session_id.into());
        self
    }

    /// Sets the initial nodes carried by a `SessionCreateRequest` for store, effect-host, and
    /// protocol implementors while materializing, executing, or persisting a session turn.
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

    /// Sets the tool access carried by a `SessionCreateRequest` for protocol and process-engine
    /// implementors while preparing or executing plugin and tool work.
    pub fn with_tool_access(mut self, tool_access: SessionToolAccess) -> Self {
        self.tool_access = tool_access;
        self
    }

    /// Sets the subagent context carried by a `SessionCreateRequest` for store, effect-host, and
    /// protocol implementors while materializing, executing, or persisting a session turn.
    pub fn with_subagent_context(mut self, subagent: SubagentSessionContext) -> Self {
        self.subagent = Some(subagent);
        self
    }

    /// Records causal provenance for protocol and process-engine implementors when the request is a
    /// child; root requests remain unchanged.
    pub fn with_caused_by(mut self, caused_by: crate::CausalRef) -> Self {
        if let SessionRelation::Child {
            caused_by: cause, ..
        } = &mut self.relation
        {
            *cause = Some(caused_by);
        }
        self
    }

    /// Sets the context overlay carried by a `SessionCreateRequest` for store, effect-host, and
    /// protocol implementors while materializing, executing, or persisting a session turn.
    pub fn with_context_overlay(mut self, context_overlay: SessionContextOverlay) -> Self {
        self.context_overlay = context_overlay;
        self
    }

    /// Labels child-session token cost for protocol and administration embedders so committed usage
    /// is attributed to the correct parent-ledger source.
    pub fn with_usage_source(mut self, usage_source: impl Into<String>) -> Self {
        self.usage_source = Some(usage_source.into());
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

    #[test]
    fn decoded_authority_rejects_missing_unknown_and_malformed_modes() {
        for value in [
            serde_json::Value::Null,
            serde_json::json!({}),
            serde_json::json!({ "mode": "unknown" }),
            serde_json::json!({ "mode": "restricted" }),
            serde_json::json!({ "mode": "ambient", "tools": [] }),
            serde_json::json!({ "mode": "restricted", "tools": "all" }),
        ] {
            assert!(
                serde_json::from_value::<SessionToolAccess>(value.clone()).is_err(),
                "malformed access unexpectedly decoded: {value}"
            );
        }
    }

    #[test]
    fn decoded_authority_uses_the_same_name_and_id_checks() {
        let duplicate_names = serde_json::json!({
            "mode": "restricted",
            "tools": [tool("tool:first", "same"), tool("tool:second", "same")]
        });
        let error = serde_json::from_value::<SessionToolAccess>(duplicate_names)
            .expect_err("duplicate names must refuse on decode");
        assert!(
            error
                .to_string()
                .contains("name `same` appears more than once")
        );

        let duplicate_ids = serde_json::json!({
            "mode": "restricted",
            "tools": [tool("tool:same", "first"), tool("tool:same", "second")]
        });
        let error = serde_json::from_value::<SessionToolAccess>(duplicate_ids)
            .expect_err("duplicate ids must refuse on decode");
        assert!(
            error
                .to_string()
                .contains("id `tool:same` appears more than once")
        );

        let duplicate_hidden = serde_json::json!({
            "mode": "ambient",
            "hidden_tools": ["hidden", "hidden"]
        });
        let error = serde_json::from_value::<SessionToolAccess>(duplicate_hidden)
            .expect_err("duplicate hidden names must refuse on decode");
        assert!(
            error
                .to_string()
                .contains("hidden tool name `hidden` appears more than once")
        );
    }

    #[test]
    fn restricted_roundtrip_preserves_complete_definition_and_opaque_null_binding() {
        let mut definition = tool("tool:restricted", "restricted");
        definition
            .manifest
            .bindings
            .insert("opaque".to_string(), serde_json::Value::Null);
        let access = SessionToolAccess::restricted([definition.clone()])
            .expect("valid restricted definition")
            .with_hidden_tools(["restricted"])
            .expect("valid hidden name");

        let encoded = serde_json::to_value(&access).expect("serialize restricted access");
        let decoded: SessionToolAccess =
            serde_json::from_value(encoded).expect("decode restricted access");
        let [decoded_definition] = decoded
            .restricted_tools()
            .expect("restricted definition survives")
        else {
            panic!("expected exactly one restricted definition")
        };
        assert_eq!(decoded_definition.manifest, definition.manifest);
        assert_eq!(
            decoded_definition.contract().input_schema,
            definition.contract().input_schema
        );
        assert_eq!(
            decoded_definition.contract().output_schema,
            definition.contract().output_schema
        );
        assert_eq!(
            decoded_definition.manifest.bindings.get("opaque"),
            Some(&serde_json::Value::Null)
        );
        assert!(decoded.hides("restricted"));
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

    #[test]
    fn create_relation_rejects_nested_observer_intent_wrappers() {
        let error = serde_json::from_value::<SessionRelation>(serde_json::json!({
            "kind": "observer_intent",
            "relation": {
                "kind": "observer_intent",
                "relation": { "kind": "root" },
                "pending_observer_process_ids": ["inner-process"]
            },
            "pending_observer_process_ids": ["outer-process"]
        }))
        .expect_err("nested observer-intent wrappers are no longer relations");
        assert_eq!(error.classify(), serde_json::error::Category::Data);
        assert!(
            error
                .to_string()
                .contains("unknown variant `observer_intent`")
        );
    }
}

#[derive(Clone)]
pub struct SessionContextOverlay {
    pub include_base_tools: bool,
}
impl Default for SessionContextOverlay {
    fn default() -> Self {
        Self {
            include_base_tools: true,
        }
    }
}
impl std::fmt::Debug for SessionContextOverlay {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionContextOverlay")
            .field("include_base_tools", &self.include_base_tools)
            .finish()
    }
}
