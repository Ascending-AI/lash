//! Immutable request envelope sources and their admission.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LlmToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: SchemaContract,
    pub output_schema: SchemaContract,
}

/// Shared instruction text whose actual retained Arc owns its byte obligation.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct RequestText(Arc<crate::allocation::Allocated<String>>);
impl RequestText {
    pub fn adopt_in(
        &self,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Self, crate::allocation::AllocationError> {
        crate::allocation::Allocated::adopt_in(&self.0, budget).map(Self)
    }
}
impl std::ops::Deref for RequestText {
    type Target = str;
    fn deref(&self) -> &str {
        &self.0
    }
}
impl From<String> for RequestText {
    fn from(text: String) -> Self {
        Self(Arc::new(text.into()))
    }
}
impl From<&str> for RequestText {
    fn from(text: &str) -> Self {
        text.to_owned().into()
    }
}
impl From<Arc<str>> for RequestText {
    fn from(text: Arc<str>) -> Self {
        text.as_ref().into()
    }
}

/// Immutable tool directory. Its actual Arc carries the directory obligation;
/// each schema carries its own prepared JSON, validator and type obligations.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct LlmToolSpecs {
    tools: Vec<LlmToolSpec>,
    #[serde(skip)]
    _allocation: crate::allocation::AllocationHold,
    #[serde(skip)]
    budget: crate::allocation::AllocationBudget,
}
impl From<Vec<LlmToolSpec>> for LlmToolSpecs {
    fn from(tools: Vec<LlmToolSpec>) -> Self {
        Self {
            tools,
            ..Self::default()
        }
    }
}
impl std::ops::Deref for LlmToolSpecs {
    type Target = Vec<LlmToolSpec>;
    fn deref(&self) -> &Self::Target {
        &self.tools
    }
}
impl AsRef<Vec<LlmToolSpec>> for LlmToolSpecs {
    fn as_ref(&self) -> &Vec<LlmToolSpec> {
        &self.tools
    }
}
impl PartialEq for LlmToolSpecs {
    fn eq(&self, other: &Self) -> bool {
        self.tools == other.tools
    }
}
impl Eq for LlmToolSpecs {}

impl LlmToolSpecs {
    pub fn allocation_budget(&self) -> &crate::allocation::AllocationBudget {
        &self.budget
    }
    pub fn adopt_tools_in(
        source: &Arc<Self>,
        budget: &crate::allocation::AllocationBudget,
    ) -> Result<Arc<Self>, crate::allocation::AllocationError> {
        if source.allocation_budget().identity() == budget.identity() {
            return Ok(Arc::clone(source));
        }
        let allocation = budget.reserve(crate::allocation::payload_capacity(
            source.as_ref(),
            std::mem::size_of::<Self>() + 16,
        ))?;
        let mut tools = Vec::with_capacity(source.len());
        for tool in source.iter() {
            tools.push(LlmToolSpec {
                name: tool.name.clone(),
                description: tool.description.clone(),
                input_schema: tool.input_schema.adopt_in(budget)?,
                output_schema: tool.output_schema.adopt_in(budget)?,
            });
        }
        Ok(Arc::new(Self {
            tools,
            _allocation: allocation,
            budget: budget.clone(),
        }))
    }
}

impl std::fmt::Display for RequestText {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self)
    }
}
