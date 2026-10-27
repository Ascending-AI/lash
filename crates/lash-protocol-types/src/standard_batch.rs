use crate::{ModelToolOutcome, ToolCallOutcome};

/// A batch member's typed result, independent of its model presentation.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BatchResultRow {
    pub index: usize,
    pub tool: String,
    #[serde(serialize_with = "crate::durable_serde::serialize")]
    pub outcome: ToolCallOutcome,
}

/// The committed member outcome with its presented text. Attachments follow
/// the wrapper's rows as separate model return parts.
#[derive(Clone, Debug, PartialEq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelBatchResultRow {
    pub index: usize,
    pub tool: String,
    pub outcome: ModelToolOutcome,
    pub body: String,
}
