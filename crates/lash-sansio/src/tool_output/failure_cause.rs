use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolFailureCause {
    Interrupted,
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
    /// Admission refused the call, or a member of its round, before any
    /// member prepared or started.
    Admission {
        refusal: crate::ToolAdmissionRefusal,
    },
    /// The body's outcome is one its recorded declaration does not admit: a
    /// Deferred without `may_defer`, or an undeclared intent kind. Nothing
    /// the outcome declared was realized.
    Declaration {
        refusal: crate::DeclarationRefusal,
    },
}
