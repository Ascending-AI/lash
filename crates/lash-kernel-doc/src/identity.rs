//! The identities of tasks and effects within a run (`K-TASK-020`,
//! `K-EFF-008`). They are derived from the document's sites, so a host can
//! tell which element of a fan-out a task or an effect belongs to.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ast::Site;

/// Which task of a run: `main`, or the task one execution of a `spawn`
/// started.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum TaskIdentity {
    Main,
    Spawned(SpawnIdentity),
}

#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SpawnIdentity {
    /// The task that ran the `spawn`.
    pub parent: Arc<TaskIdentity>,
    /// The site of the `spawn`.
    pub site: Site,
    /// How many times the parent had run that site before, from 0.
    pub occurrence: u64,
}

/// Which wait of a run: one execution of a `perform` or a `sleep`.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct EffectIdentity {
    pub task: TaskIdentity,
    /// The site of the `perform` or the `sleep`.
    pub site: Site,
    /// How many times the task had run that site before, from 0.
    pub occurrence: u64,
    /// The loops the task was inside when it ran the site, outermost first,
    /// across its active calls.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<LoopIteration>,
}

/// One enclosing loop and the iteration it was on.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct LoopIteration {
    /// The site of the `for` or the `while`.
    pub site: Site,
    /// The iteration, from 0.
    pub iteration: u64,
}
