#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub enum ToolCatalogBuildError {
    /// A tool's operation is no canonical operation.
    InvalidOperation {
        tool_id: crate::ToolId,
        message: String,
    },
    UnusableSchema {
        tool_id: crate::ToolId,
        operation: String,
        purpose: crate::SchemaPurpose,
        source: crate::SchemaAdmissionError,
    },
    /// The tool supplied no output type, including the empty schema.
    MissingOutputDeclaration {
        tool_id: crate::ToolId,
        operation: String,
    },
    MissingContract {
        tool_id: crate::ToolId,
        operation: String,
    },
    DuplicateId {
        tool_id: crate::ToolId,
    },
    /// Two tools are performed as one operation.
    DuplicateOperation {
        operation: String,
        first_tool: crate::ToolId,
        second_tool: crate::ToolId,
    },
    /// A member is refused against the deployment it registers in.
    RegistrationRefused {
        refusal: crate::RegistrationRefused,
    },
}

impl std::fmt::Display for ToolCatalogBuildError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidOperation { tool_id, message } => {
                write!(formatter, "tool `{tool_id}` names no operation: {message}")
            }
            Self::UnusableSchema {
                tool_id,
                operation,
                purpose,
                source,
            } => {
                write!(
                    formatter,
                    "tool `{operation}` ({tool_id}) has unusable {purpose:?} schema: {source}"
                )
            }
            Self::MissingOutputDeclaration { tool_id, operation } => {
                write!(
                    formatter,
                    "tool `{operation}` ({tool_id}) must declare an output type; use a named Any schema for unconstrained values"
                )
            }
            Self::MissingContract { tool_id, operation } => {
                write!(
                    formatter,
                    "resident tool `{operation}` ({tool_id}) has no contract"
                )
            }
            Self::DuplicateId { tool_id } => {
                write!(formatter, "resident catalog repeats tool id `{tool_id}`")
            }
            Self::DuplicateOperation {
                operation,
                first_tool,
                second_tool,
            } => {
                write!(
                    formatter,
                    "tools `{first_tool}` and `{second_tool}` are both performed as `{operation}`; one operation names one tool"
                )
            }
            Self::RegistrationRefused { refusal } => write!(formatter, "{refusal}"),
        }
    }
}

impl std::error::Error for ToolCatalogBuildError {}
