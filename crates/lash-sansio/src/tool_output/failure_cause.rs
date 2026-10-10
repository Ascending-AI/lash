use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolFailureCause {
    Interrupted,
    /// The host body panicked. It may already have performed outside work;
    /// this terminal is never retried or sent back to the model for repair.
    Panicked {
        tool_name: String,
        call_id: crate::ToolCallId,
        message: String,
    },
    /// An admitted engine step has no registered body on this deployment.
    /// Its exact registration refusal is retained in the failure's raw value.
    EngineStepRegistrationUnavailable {
        engine: String,
        step: String,
        step_kind: String,
    },
    /// An engine step could not reconstruct its process's catalog. The
    /// plugin error is retained as a typed command failure in the raw value.
    EngineStepCatalogUnreadable {
        engine: String,
        step: String,
        step_kind: String,
    },
    /// A body for an engine step was requested for a non-engine process.
    EngineStepWithoutEngine {
        step: String,
        step_kind: String,
    },
    ExecutionLimit {
        cause: crate::LimitCause,
    },
    /// A result check proposed state commands outside the recorded body
    /// that owns its decision. No command was reduced or published.
    PluginStateUnrecorded {
        plugin: String,
    },
    ToolSchemaAdmission {
        source: Box<crate::ToolCatalogBuildError>,
    },
    SchemaAdmission {
        source: crate::SchemaAdmissionError,
    },
    ValueMismatch {
        source: crate::ValueMismatch,
    },
    /// The body's outcome is one its recorded declaration does not admit: a
    /// Deferred without `may_defer`, an undeclared intent kind, or an
    /// undeclared turn control. Nothing
    /// the outcome declared was realized.
    Declaration {
        refusal: crate::DeclarationRefusal,
    },
    /// A control call made from a task the cell spawned. Only the cell's
    /// main task ends the turn; the body did not run.
    ControlFromSpawnedTask,
    /// A control call made while a task the cell started is outstanding or
    /// its result unobserved. The body did not run.
    ControlWithOutstandingTasks,
    /// The iteration's one control attempt is spent: an earlier control
    /// call was already admitted. The body did not run.
    ControlAttemptSpent,
    /// A call beside this control call in its round failed or was
    /// cancelled, so the turn does not end on it. The body did not run.
    ControlSiblingFailed,
}
