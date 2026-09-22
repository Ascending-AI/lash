pub use lash_core_store::session_identity::{
    AgentFrameAssignment, AgentFrameReason, AgentFrameRecord, FrameNodeId, FrameNodeIdError,
    OpenAgentFrameRequest, OpenAgentFrameResult, SessionLineage, SessionObservedProcessOutcome,
    SessionObservedProcessReceipt, SessionObserverIntent, SessionRelation, SessionSnapshot,
    SessionStartPoint, SessionToolAccess, SessionToolAccessError, SubagentSessionContext,
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
    ParentFork,
}

/// Serialized upper bound on a captured [`SessionPluginInit`]. The payload
/// rides inside the durable creation request (process rows, trigger targets,
/// remote protocol), so a single capture cannot exceed the row budget a
/// durable journal carries. Captures larger than this are refused with
/// [`PluginError::SessionInitTooLarge`] rather than truncated.
pub const SESSION_PLUGIN_INIT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// The spawn site records exactly what the parent's [`PluginSession`]
/// used to read when forking: the parent's plugin state, its tool-catalog
/// overlay, and the
/// exported tool state. The capture is taken once, travels inside the durable
/// [`SessionCreateRequest`], and is what the materializer hands to plugin
/// session construction — a worker restart between spawn and execution
/// initializes byte-for-byte identically, and post-spawn parent mutations are
/// invisible to the peer.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionPluginInit {
    pub plugin_state: crate::PluginState,
    pub tool_catalog_overlay: crate::ToolCatalogContribution,
    pub tool_state: crate::ToolState,
}

impl SessionPluginInit {
    /// Builds a bounded capture; payloads serialized beyond
    /// [`SESSION_PLUGIN_INIT_MAX_BYTES`] are refused.
    pub fn captured(
        plugin_state: crate::PluginState,
        tool_catalog_overlay: crate::ToolCatalogContribution,
        tool_state: crate::ToolState,
    ) -> Result<Self, PluginError> {
        let init = Self {
            plugin_state,
            tool_catalog_overlay,
            tool_state,
        };
        let bytes = serde_json::to_vec(&init).map_err(|err| {
            PluginError::Session(format!("session plugin init failed to serialize: {err}"))
        })?;
        if bytes.len() > SESSION_PLUGIN_INIT_MAX_BYTES {
            return Err(PluginError::SessionInitTooLarge {
                bytes: bytes.len(),
                limit: SESSION_PLUGIN_INIT_MAX_BYTES,
            });
        }
        Ok(init)
    }
}

#[cfg(test)]
mod session_plugin_init_tests {
    use super::{SESSION_PLUGIN_INIT_MAX_BYTES, SessionPluginInit};
    use crate::PluginError;

    fn oversize_plugin_state() -> crate::PluginState {
        let mut plugins = std::collections::BTreeMap::new();
        let mut values = std::collections::BTreeMap::new();
        values.insert(
            "blob".to_string(),
            serde_json::Value::String("x".repeat(SESSION_PLUGIN_INIT_MAX_BYTES)),
        );
        plugins.insert(
            "fat-plugin".to_string(),
            crate::PluginNamespaceState {
                generation: 0,
                values,
            },
        );
        crate::PluginState { plugins }
    }

    #[test]
    fn capture_refuses_payloads_beyond_the_bound() {
        let err = SessionPluginInit::captured(
            oversize_plugin_state(),
            crate::ToolCatalogContribution::default(),
            crate::ToolState::default(),
        )
        .expect_err("oversize capture must be refused");

        assert!(
            matches!(err, PluginError::SessionInitTooLarge { .. }),
            "expected SessionInitTooLarge, got {err:?}"
        );
    }

    #[test]
    fn capture_serializes_the_durable_payload_shape() {
        let init = SessionPluginInit::captured(
            crate::PluginState::default(),
            crate::ToolCatalogContribution::default(),
            crate::ToolState::default(),
        )
        .expect("empty capture");

        let bytes = serde_json::to_vec(&init).expect("serialize");
        let roundtrip: SessionPluginInit = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(roundtrip.plugin_state, init.plugin_state);
    }
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
    /// Plugin-owned options that configure plugin behavior at session
    /// creation time. Each plugin decodes only the entry keyed by its id.
    #[serde(default)]
    pub plugin_options: PluginOptions,
    /// Required when `plugin_source` is [`SessionPluginSource::ParentFork`]; ignored
    /// otherwise.
    /// Materialization initializes the peer from this payload alone and never reads a live
    /// parent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin_init: Option<SessionPluginInit>,
}

impl SessionCreateRequest {
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
            plugin_options,
            plugin_init: None,
        }
    }

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
            plugin_options,
            plugin_init: None,
        }
    }

    pub fn child(
        parent_session_id: impl Into<SessionId>,
        start: SessionStartPoint,
        policy: SessionPolicy,
        plugin_options: PluginOptions,
    ) -> Self {
        Self {
            session_id: Some(SessionId::from(uuid::Uuid::new_v4().to_string())),
            relation: SessionRelation::Child {
                parent_session_id: parent_session_id.into(),
                caused_by: None,
            },
            start,
            policy: Some(policy),
            plugin_source: SessionPluginSource::CurrentHostFresh,
            initial_nodes: Vec::new(),
            observed_processes: Vec::new(),
            tool_access: SessionToolAccess::default(),
            subagent: None,
            plugin_options,
            plugin_init: None,
        }
    }

    pub fn with_plugin_source(mut self, plugin_source: SessionPluginSource) -> Self {
        self.plugin_source = plugin_source;
        self
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

    pub fn with_subagent_context(mut self, subagent: SubagentSessionContext) -> Self {
        self.subagent = Some(subagent);
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

    /// Attaches the spawn-time plugin init capture carried by a
    /// `SessionCreateRequest` for store and process-engine implementors while
    /// preparing or materializing a forked session.
    pub fn with_plugin_init(mut self, plugin_init: SessionPluginInit) -> Self {
        self.plugin_init = Some(plugin_init);
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
