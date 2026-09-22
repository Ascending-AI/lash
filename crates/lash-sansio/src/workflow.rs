use serde::{Deserialize, Serialize};

/// Stable source-level location of one runtime site under a workflow node.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct WorkflowExecutionSite {
    pub owner: String,
    #[serde(default)]
    pub path: Vec<u32>,
    pub kind: String,
    pub label: String,
}

impl WorkflowExecutionSite {
    pub fn new(
        owner: impl Into<String>,
        path: impl AsRef<[u32]>,
        kind: impl Into<String>,
        label: impl Into<String>,
    ) -> Self {
        Self {
            owner: owner.into(),
            path: path.as_ref().to_vec(),
            kind: kind.into(),
            label: label.into(),
        }
    }
}
