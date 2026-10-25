use crate::SchemaContract;

/// A tool's explicit output declaration. Unlike a general schema contract,
/// this cannot contain the empty schema or acquire one through a default.
/// Author it through [`super::ToolDefinition::raw`] or
/// [`super::ToolDefinition::typed`].
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct ToolOutputSchema(pub(super) SchemaContract);

impl ToolOutputSchema {
    pub(super) fn admit(schema: SchemaContract) -> Option<Self> {
        if schema
            .canonical()
            .as_object()
            .is_some_and(serde_json::Map::is_empty)
        {
            None
        } else {
            Some(Self(schema))
        }
    }

    /// The admitted schema and its provider projections.
    pub fn as_contract(&self) -> &SchemaContract {
        &self.0
    }
}

impl std::ops::Deref for ToolOutputSchema {
    type Target = SchemaContract;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'de> serde::Deserialize<'de> for ToolOutputSchema {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let schema = <SchemaContract as serde::Deserialize>::deserialize(deserializer)?;
        Self::admit(schema).ok_or_else(|| serde::de::Error::custom(
            "a tool output must declare a type; use a named Any schema for unconstrained values"
        ))
    }
}
