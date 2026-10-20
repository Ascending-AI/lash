use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolFailureCause {
    /// A host-configured aggregate callback deadline passed without an answer.
    ControlPhaseExpired {
        expiry: Box<crate::ControlPhaseExpired>,
    },
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
    /// A start the body declared names an environment its attempt did not
    /// hold through `AttemptContext::declare_launch` (D10). Nothing the
    /// outcome declared was realized.
    LaunchNotHeld {
        env_ref: String,
    },
    /// A process step's linked tool is missing from the provider its env
    /// links it to on this node (D3). The body did not run.
    LinkedToolMissing {
        effect: String,
        tool: crate::ToolId,
    },
    /// A call's linked tool is served on this node under another contract
    /// id than the call is pinned to (`operation_contract_changed`). The
    /// body did not run.
    OperationContractChanged {
        effect: String,
        tool: crate::ToolId,
        pinned: lash_kernel_doc::ContractId,
        live: lash_kernel_doc::ContractId,
    },
    /// A process's `call_dynamic` call whose input is not a call path with
    /// its arguments. Nothing was resolved.
    DynamicCallInvalid {
        reason: String,
    },
    /// The deferred provider refused a process's `call_dynamic` path. The
    /// decision is recorded and never resolved again; no tool ran.
    DynamicCallRefused {
        path: String,
        reason: String,
    },
    /// No tool is available for a process's `call_dynamic` path, with the
    /// host's note. The decision is recorded and never resolved again; no
    /// tool ran.
    DynamicCallUnavailable {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
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
    /// A workflow's body ended in an error no `catch` took (FIG-6810). The
    /// failure's message is the error's own message, and its raw value the
    /// error's data: for a failed tool call, the tool's failure.
    UncaughtError {
        /// The error's kind as the body raised it (`type_error`,
        /// `tool_failed`), or `None` when the body threw a value that is
        /// not an error.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_kind: Option<String>,
        /// Where the run's main task raised it, in the document the process
        /// runs: the `perform`, `sleep` or `join` whose outcome raised, else
        /// the statement whose evaluation raised, a library call's included.
        /// An error out of library code, an `await` of a failed task's among
        /// them, names the first document statement it unwinds into, and one
        /// a `finally` held names its `try` statement. `None` when a run-root
        /// cleanup held it (a Python entry that started tasks), or no
        /// document statement was left to name it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site: Option<crate::Site>,
    },
    /// A value retained out of history was asked for whole, and the
    /// attachment that held it is no longer stored: its shape is all history
    /// keeps of it (FIG-1439).
    RetainedValueUnavailable {
        attachment: crate::AttachmentId,
    },
}
