use lash_sansio::core_support::Blake3DomainHasher;
use serde::{Deserialize, Serialize};

use crate::{ModuleRef, ProcessRef};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LashlangExecutionContext {
    pub(crate) module_ref: ModuleRef,
    pub(crate) entry: LashlangExecutionEntry,
}

impl LashlangExecutionContext {
    pub(crate) fn main(module_ref: ModuleRef) -> Self {
        Self {
            module_ref,
            entry: LashlangExecutionEntry::Main,
        }
    }

    pub(crate) fn process(
        module_ref: ModuleRef,
        process_ref: ProcessRef,
        process_name: impl Into<String>,
    ) -> Self {
        Self {
            module_ref,
            entry: LashlangExecutionEntry::Process {
                process_ref,
                process_name: process_name.into(),
            },
        }
    }

    pub(crate) fn builder(&self) -> LashlangExecutionSiteBuilder<'_> {
        LashlangExecutionSiteBuilder { context: self }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LashlangExecutionEntry {
    Main,
    Process {
        process_ref: ProcessRef,
        process_name: String,
    },
}

impl LashlangExecutionEntry {
    fn stable_key(&self) -> String {
        match self {
            Self::Main => "main".to_string(),
            Self::Process { process_ref, .. } => process_ref_key(process_ref),
        }
    }

    fn workflow_owner(&self) -> String {
        match self {
            Self::Main => "main".to_string(),
            Self::Process { process_name, .. } => format!("process:{process_name}"),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) struct LashlangAstPath(Vec<u32>);

impl LashlangAstPath {
    pub(crate) fn from_indices(indices: &[u32]) -> Self {
        Self(indices.to_vec())
    }

    fn stable_text(&self) -> String {
        if self.0.is_empty() {
            return "root".to_string();
        }
        self.0
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(".")
    }

    fn indices(&self) -> &[u32] {
        &self.0
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LashlangExecutionSiteBuilder<'context> {
    context: &'context LashlangExecutionContext,
}

impl LashlangExecutionSiteBuilder<'_> {
    pub(crate) fn node_site(
        &self,
        path: &LashlangAstPath,
        kind: impl Into<String>,
        label: impl Into<String>,
    ) -> LashlangExecutionSite {
        let kind = kind.into();
        let label = label.into();
        LashlangExecutionSite {
            node_id: self.node_id(path, &kind),
            node_kind: kind.clone(),
            label: label.clone(),
            branch: None,
            workflow_site: WorkflowExecutionSite::new(
                self.context.entry.workflow_owner(),
                path.indices(),
                kind,
                label,
            ),
        }
    }

    pub(crate) fn branch_site(&self, path: &LashlangAstPath) -> LashlangExecutionSite {
        LashlangExecutionSite {
            node_id: self.node_id(path, "branch"),
            node_kind: "branch".to_string(),
            label: "if".to_string(),
            branch: Some(LashlangBranchSite {
                then_edge_id: self.branch_edge_id(path, ProcessBranchSelection::Then),
                else_edge_id: self.branch_edge_id(path, ProcessBranchSelection::Else),
            }),
            workflow_site: WorkflowExecutionSite::new(
                self.context.entry.workflow_owner(),
                path.indices(),
                "branch",
                "if",
            ),
        }
    }

    pub(crate) fn branch_edge_id(
        &self,
        path: &LashlangAstPath,
        selection: ProcessBranchSelection,
    ) -> String {
        let label = match selection {
            ProcessBranchSelection::Then => "then",
            ProcessBranchSelection::Else => "else",
        };
        stable_id(
            "edge",
            &[
                self.context.module_ref.to_string(),
                self.context.entry.stable_key(),
                path.stable_text(),
                "branch".to_string(),
                label.to_string(),
            ],
        )
    }

    fn node_id(&self, path: &LashlangAstPath, kind: impl AsRef<str>) -> String {
        stable_id(
            kind.as_ref(),
            &[
                self.context.module_ref.to_string(),
                self.context.entry.stable_key(),
                path.stable_text(),
                kind.as_ref().to_string(),
            ],
        )
    }
}

/// Stable source-level location carried by runtime sites for workflow-graph joins.
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

    pub(crate) fn same_location(&self, other: &Self) -> bool {
        self.owner == other.owner && self.path == other.path
    }
}

fn stable_id(prefix: &str, parts: &[String]) -> String {
    let mut hasher = Blake3DomainHasher::new("lash-lashlang-execution-site/v2");
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let hash = hasher.finalize_hex();
    format!("{prefix}:{}", &hash[..24])
}

pub fn process_ref_key(process_ref: &ProcessRef) -> String {
    format!("{}:{}", process_ref.component, process_ref.pos)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashlangExecutionSite {
    pub node_id: String,
    pub node_kind: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<LashlangBranchSite>,
    pub workflow_site: WorkflowExecutionSite,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangExecutionCallSite {
    pub site: LashlangExecutionSite,
    pub occurrence: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashlangBranchSite {
    pub then_edge_id: String,
    pub else_edge_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessBranchSelection {
    Then,
    Else,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangExecutionChild {
    pub process_id: String,
    pub module_ref: ModuleRef,
    pub process_ref: ProcessRef,
    pub process_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LashlangExecutionObservation {
    NodeStarted {
        site: LashlangExecutionSite,
        occurrence: u64,
    },
    NodeCompleted {
        site: LashlangExecutionSite,
        occurrence: u64,
    },
    NodeFailed {
        site: LashlangExecutionSite,
        occurrence: u64,
        error: String,
    },
    BranchSelected {
        site: LashlangExecutionSite,
        occurrence: u64,
        edge_id: String,
        selected: ProcessBranchSelection,
    },
    ChildStarted {
        site: LashlangExecutionSite,
        occurrence: u64,
        child: LashlangExecutionChild,
    },
}
