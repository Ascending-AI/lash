use super::*;

#[derive(Clone)]
pub struct ToolCatalogContext {
    pub owner: crate::RuntimeOwner,
    pub tools: Vec<ToolManifest>,
    pub resolve_contract: Option<lash_sansio::ToolContractResolver>,
    pub tool_access: SessionToolAccess,
    pub subagent: Option<SubagentSessionContext>,
    pub extensions: PluginExtensions,
}

/// A tool check's request to stop the owning logical Run (FIG-1399).
///
/// The code is the plugin's own spelling; the runtime namespaces it under the
/// plugin that returned it, so no plugin can abort in another's name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginAbort {
    pub code: String,
    pub message: String,
}

impl PluginAbort {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// The abort's code under `plugin_id`'s namespace.
    ///
    /// The plugin id is the namespace when it satisfies namespace validation;
    /// a plugin id that cannot be a namespace falls back to the shared
    /// `plugin` namespace, keeping the spelling verbatim either way.
    #[expect(
        clippy::expect_used,
        reason = "the literal `plugin` is a fixed valid host-namespace spelling, so validation cannot fail"
    )]
    pub fn failure_code(&self, plugin_id: &str) -> crate::FailureCode {
        let namespace = lash_sansio::Namespace::host(plugin_id).unwrap_or_else(|_| {
            lash_sansio::Namespace::host("plugin").expect("`plugin` is a valid host namespace")
        });
        crate::FailureCode::foreign(namespace, self.code.clone())
            .expect("a validated host namespace is foreign-mintable")
    }
}

/// What one before-turn or checkpoint observer contributes: messages the
/// turn sees, runtime events the session publishes, and commands against
/// its own plugin state namespace. Contributions are applied at the turn's
/// owned boundary; an observer has no veto.
#[derive(Clone, Debug, Default)]
pub struct TurnContributions {
    pub messages: Vec<PluginMessage>,
    pub events: Vec<PluginRuntimeEvent>,
    /// Published with the callback's recorded decision (K10).
    pub state: super::StateCommands,
}

/// What one after-turn observer contributes, applied before the turn's
/// final commit.
#[derive(Clone, Debug, Default)]
pub struct AfterTurnContributions {
    pub messages: Vec<PluginMessage>,
    pub events: Vec<PluginRuntimeEvent>,
    /// Durable plugin records appended to the turn's graph, outside the
    /// conversation.
    pub records: Vec<PluginRecordContribution>,
    /// Published with the callback's recorded decision (K10).
    pub state: super::StateCommands,
}

/// A durable plugin record an after-turn observer appends to the turn.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginRecordContribution {
    pub plugin_type: String,
    pub body: serde_json::Value,
}

/// One before-turn or after-turn observer's recorded decision: what it
/// contributed apart from its state commands, whose resolution the same
/// recorded step carries. Replay serves it without running the observer.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedTurnContribution {
    pub plugin_id: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub messages: Vec<PluginMessage>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<PluginRuntimeEvent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<PluginRecordContribution>,
}

#[derive(Clone, Debug, Default)]
pub struct TurnPreparation {
    pub messages: crate::MessageSequence,
    pub events: Vec<crate::SessionStreamEvent>,
}

#[derive(Clone, Debug, Default)]
pub struct CheckpointApplication {
    pub messages: Vec<PluginMessage>,
    pub events: Vec<crate::SessionStreamEvent>,
}

#[derive(Clone, Debug)]
pub struct TurnFinalization {
    pub turn: AssembledTurn,
    pub events: Vec<crate::SessionStreamEvent>,
}

/// Publishes a plugin's runtime events as session observations under
/// `cursor`'s lane — synchronous, never awaited (ADR 0105 §1).
pub fn observe_plugin_runtime_events(
    cursor: &mut crate::engine::ObservationCursor,
    observer: &dyn crate::engine::ObservationSink,
    plugin_id: &str,
    events: Vec<PluginRuntimeEvent>,
) {
    for event in plugin_runtime_session_events(plugin_id, events) {
        cursor.observe(observer, crate::engine::ObservedEvent::Session(event));
    }
}

pub fn plugin_runtime_session_events(
    plugin_id: &str,
    events: Vec<PluginRuntimeEvent>,
) -> Vec<crate::SessionStreamEvent> {
    events
        .into_iter()
        .map(|event| crate::SessionStreamEvent::PluginEvent {
            plugin_id: plugin_id.to_string(),
            event,
        })
        .collect()
}
