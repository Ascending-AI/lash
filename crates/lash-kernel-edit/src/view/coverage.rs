//! The host's one interpretation of run counts and verified paths.

use super::WorkflowView;
use lash_kernel_doc::{RanCounts, ReplayPath, RunObservation, Site, TaskIdentity};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkflowCoverage {
    pub observation: Option<RunObservation>,
    pub statements: Vec<StatementCoverage>,
    pub scopes: Vec<ScopeCoverage>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatementCoverage {
    pub site: Site,
    /// None is unobserved; Some(0) is an observed statement that did not run.
    pub runs: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScopeCoverage {
    pub block: Site,
    /// Arm selections, loop iterations, caught errors or function calls.
    pub entries: Option<u64>,
}

/// Count statement starts and scope entries for this view. The caller supplies
/// counts from the same document; starts elsewhere in that document are ignored.
/// Observation is authoritative: off and a missing start never manufacture zeros.
pub fn coverage(view: &WorkflowView, counts: &RanCounts) -> WorkflowCoverage {
    let mut ran = BTreeMap::<u32, u64>::new();
    for (ordinal, count) in &counts.ran {
        let total = ran.entry(*ordinal).or_default();
        *total = total.saturating_add(*count);
    }
    let count = |ordinal| {
        (counts.observation == Some(RunObservation::Counts))
            .then(|| ran.get(&ordinal).copied().unwrap_or(0))
    };
    let first = first_statements(view);
    WorkflowCoverage {
        observation: counts.observation,
        statements: view
            .statements
            .iter()
            .map(|statement| StatementCoverage {
                site: statement.site.clone(),
                runs: count(statement.ordinal),
            })
            .collect(),
        scopes: view
            .scopes
            .iter()
            .map(|scope| ScopeCoverage {
                block: scope.block.clone(),
                entries: count(first.get(&scope.block).copied().unwrap_or(scope.ordinal)),
            })
            .collect(),
    }
}

fn first_statements(view: &WorkflowView) -> BTreeMap<Site, u32> {
    let mut first = BTreeMap::new();
    for statement in &view.statements {
        first
            .entry(statement.block.clone())
            .or_insert(statement.ordinal);
    }
    first
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

/// Map a verified replay to sites in this view, retaining task order and each
/// task's start order. A nonempty scope enters immediately before its first
/// statement; an empty scope has its own start. Ordinals of other units are
/// omitted; derive those units' views to read their paths.
pub fn path(view: &WorkflowView, replay: &ReplayPath) -> Vec<PathStep> {
    let mut sites = BTreeMap::<u32, Vec<PathSite>>::new();
    let first = first_statements(view);
    for scope in &view.scopes {
        sites
            .entry(first.get(&scope.block).copied().unwrap_or(scope.ordinal))
            .or_default()
            .push(PathSite::Scope {
                block: scope.block.clone(),
            });
    }
    for statement in &view.statements {
        sites
            .entry(statement.ordinal)
            .or_default()
            .push(PathSite::Statement {
                site: statement.site.clone(),
            });
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
