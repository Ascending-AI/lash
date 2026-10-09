use crate::{ExecutionNodeKind, WorkflowSitePath};
use serde::{Deserialize, Serialize};

/// One execution site of a workflow node: the node (`owner` and `path`), the
/// exact executable subexpression inside its statement (`site_path`), and a
/// description of what runs there. `kind` and `label` describe the site; they
/// are not its identity.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, schemars::JsonSchema,
)]
pub struct WorkflowExecutionSite {
    pub owner: String,
    #[serde(default)]
    pub path: Vec<u32>,
    #[serde(default, skip_serializing_if = "WorkflowSitePath::is_empty")]
    pub site_path: WorkflowSitePath,
    pub kind: ExecutionNodeKind,
    pub label: String,
}

impl WorkflowExecutionSite {
    pub fn new(
        owner: impl Into<String>,
        path: impl AsRef<[u32]>,
        kind: ExecutionNodeKind,
        label: impl Into<String>,
    ) -> Self {
        Self {
            owner: owner.into(),
            path: path.as_ref().to_vec(),
            site_path: WorkflowSitePath::default(),
            kind,
            label: label.into(),
        }
    }

    /// This site at `site_path` inside its node's statement.
    #[must_use]
    pub fn at(mut self, site_path: WorkflowSitePath) -> Self {
        self.site_path = site_path;
        self
    }
}
