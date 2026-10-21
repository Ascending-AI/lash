//! The host's one interpretation of run counts and verified paths.

use lash_kernel_doc::Document;
use lash_kernel_doc::Node;
use lash_vocabulary::observation::RanCounts;
use lash_vocabulary::observation::ReplayPath;
use lash_vocabulary::observation::RunObservation;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::TaskLabel;
use lash_vocabulary::sites::Unit;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Statement starts, scope entries, action issues and while tests of one entry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowCoverage {
    /// The observation the counts came under; absent is an unobserved run.
    pub observation: Option<RunObservation>,
    /// Each statement of the entry, in document order.
    pub statements: Vec<StatementCoverage>,
    /// Issues at exact action sites, independent of statement starts.
    pub actions: Vec<StatementCoverage>,
    /// While header evaluations, including the final false test.
    pub conditions: Vec<StatementCoverage>,
    /// Each block of the entry, in document order.
    pub scopes: Vec<ScopeCoverage>,
}

/// One exact statement, action or while-test coordinate and its observed count.
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
    let nodes = entry_nodes(document, entry);
    WorkflowCoverage {
        observation: counts.observation,
        actions: document
            .action_ordinals()
            .into_iter()
            .filter(|(site, _)| site.unit == *entry)
            .map(|(site, ordinal)| StatementCoverage {
                site,
                runs: count(ordinal),
            })
            .collect(),
        conditions: document
            .condition_ordinals()
            .into_iter()
            .filter(|(site, _)| site.unit == *entry)
            .map(|(site, ordinal)| StatementCoverage {
                site,
                runs: count(ordinal),
            })
            .collect(),
        statements: nodes
            .statements
            .iter()
            .map(|(site, ordinal)| StatementCoverage {
                site: site.clone(),
                runs: count(*ordinal),
            })
            .collect(),
        scopes: nodes
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
    pub task: TaskLabel,
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

/// Coverage ready for a coarse node; loop iterations remain arm entries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeCoverage {
    pub site: Site,
    /// Action issues, control starts, or code-group entries (first member only).
    pub runs: Option<u64>,
    pub condition_tests: Option<u64>,
    pub arms: Vec<ScopeCoverage>,
}

impl crate::WorkflowView {
    /// Read counts collected for this view's document. No host-side grouping,
    /// copying a statement count onto actions, or summing code members.
    pub fn coverage(&self, counts: &RanCounts) -> Vec<NodeCoverage> {
        if self.callables.is_empty() {
            return Vec::new();
        }
        let mut ran = BTreeMap::<u32, u64>::new();
        for (ordinal, count) in &counts.ran {
            *ran.entry(*ordinal).or_default() += count;
        }
        let count = |ordinal: Option<u32>| {
            (counts.observation == Some(RunObservation::Counts)).then(|| {
                ordinal
                    .and_then(|ordinal| ran.get(&ordinal).copied())
                    .unwrap_or(0)
            })
        };
        // The document count helper carries the same canonical action order;
        // the ordinals are stored on the view rather than re-derived in a host.
        self.nodes
            .iter()
            .map(|node| NodeCoverage {
                site: node.site.clone(),
                runs: count(Some(node.ordinal)),
                condition_tests: node
                    .condition_ordinal
                    .and_then(|ordinal| count(Some(ordinal))),
                arms: node
                    .arms
                    .iter()
                    .map(|arm| {
                        let first = self
                            .statements
                            .iter()
                            .find(|statement| statement.block == arm.block)
                            .map(|statement| statement.ordinal)
                            .or_else(|| {
                                self.scopes
                                    .iter()
                                    .find(|scope| scope.block == arm.block)
                                    .map(|scope| scope.ordinal)
                            });
                        ScopeCoverage {
                            block: arm.block.clone(),
                            entries: count(first),
                        }
                    })
                    .collect(),
            })
            .collect()
    }
}
