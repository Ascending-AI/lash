//! The host's one interpretation of run counts and verified paths.

use lash_kernel_doc::{
    Document, Node, RanCounts, ReplayPath, RunObservation, Site, TaskIdentity, Unit,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Statement starts and scope entries of one entry of a document.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCoverage {
    /// The observation the counts came under; absent is an unobserved run.
    pub observation: Option<RunObservation>,
    /// Each statement of the entry, in document order.
    pub statements: Vec<StatementCoverage>,
    /// Each block of the entry, in document order.
    pub scopes: Vec<ScopeCoverage>,
}

/// One statement of the entry and how often it ran.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StatementCoverage {
    pub site: Site,
    /// None is unobserved; Some(0) is an observed statement that did not run.
    pub runs: Option<u64>,
}

/// One block of the entry and how often a run entered it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ScopeCoverage {
    pub block: Site,
    /// Arm selections, loop iterations, caught errors or function calls.
    pub entries: Option<u64>,
}

/// Count statement starts and scope entries for this entry of `document`.
/// The caller supplies counts from the same document; starts elsewhere in
/// that document are ignored. Observation is authoritative: off and a
/// missing start never manufacture zeros. An `entry` the document does not
/// declare, or [`Unit::Library`], yields empty statements and scopes.
pub fn coverage(document: &Document, entry: &Unit, counts: &RanCounts) -> WorkflowCoverage {
    let mut ran = BTreeMap::<u32, u64>::new();
    for (ordinal, count) in &counts.ran {
        let total = ran.entry(*ordinal).or_default();
        *total = total.saturating_add(*count);
    }
    let count = |ordinal| {
        (counts.observation == Some(RunObservation::Counts))
            .then(|| ran.get(&ordinal).copied().unwrap_or(0))
    };
    let entry = entry_nodes(document, entry);
    WorkflowCoverage {
        observation: counts.observation,
        statements: entry
            .statements
            .iter()
            .map(|(site, ordinal)| StatementCoverage {
                site: site.clone(),
                runs: count(*ordinal),
            })
            .collect(),
        scopes: entry
            .scopes
            .iter()
            .map(|(block, entered)| ScopeCoverage {
                block: block.clone(),
                entries: count(*entered),
            })
            .collect(),
    }
}

/// The statements and blocks of one unit of a document as run counts and
/// replay paths address them: each statement's site with its ordinal, and
/// each block's site with the ordinal a run entering it reports — its
/// first statement's for a nonempty block, its own for an empty one.
struct EntryNodes {
    statements: Vec<(Site, u32)>,
    scopes: Vec<(Site, u32)>,
}

fn entry_nodes(document: &Document, entry: &Unit) -> EntryNodes {
    let mut nodes = EntryNodes {
        statements: Vec::new(),
        scopes: Vec::new(),
    };
    for (ordinal, (site, node)) in document.ordinal_nodes().into_iter().enumerate() {
        if site.unit != *entry {
            continue;
        }
        let ordinal = ordinal as u32;
        match node {
            Node::Stmt(_) => nodes.statements.push((site, ordinal)),
            Node::Block(block) => nodes.scopes.push((
                site,
                if block.is_empty() {
                    ordinal
                } else {
                    ordinal + 1
                },
            )),
            Node::Action(_) | Node::Expr(_) => {}
        }
    }
    nodes
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathStep {
    pub task: TaskIdentity,
    #[serde(flatten)]
    pub site: PathSite,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PathSite {
    Statement { site: Site },
    Scope { block: Site },
}

/// Map a verified replay to sites in this entry of `document`, retaining
/// task order and each task's start order. A nonempty scope enters
/// immediately before its first statement; an empty scope has its own
/// start. Ordinals of other units are omitted; pass that unit to read its
/// path.
pub fn path(document: &Document, entry: &Unit, replay: &ReplayPath) -> Vec<PathStep> {
    let mut sites = BTreeMap::<u32, Vec<PathSite>>::new();
    let entry = entry_nodes(document, entry);
    for (block, entered) in &entry.scopes {
        sites.entry(*entered).or_default().push(PathSite::Scope {
            block: block.clone(),
        });
    }
    for (site, ordinal) in &entry.statements {
        sites
            .entry(*ordinal)
            .or_default()
            .push(PathSite::Statement { site: site.clone() });
    }
    let mut steps = Vec::new();
    for task in &replay.tasks {
        for ordinal in &task.starts {
            for site in sites.get(ordinal).into_iter().flatten() {
                steps.push(PathStep {
                    task: task.task.clone(),
                    site: site.clone(),
                });
            }
        }
    }
    steps
}
