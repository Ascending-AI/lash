//! Derived run observations, addressed by document site ordinal.

use serde::{Deserialize, Serialize};

/// Per-run observation. Off is unobserved, never a zero count.
#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum RunObservation {
    #[default]
    Off,
    Counts,
}

impl RunObservation {
    /// Whether this setting records no counts.
    pub fn is_off(&self) -> bool {
        *self == Self::Off
    }
}

/// Exact semantic starts, action issues and condition tests by document ordinal.
/// Deltas add across frames and committed segments; they never count VM steps.
/// The transparent encoding keeps observations independent of the running engine.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(transparent)]
pub struct SiteCounts(Vec<(u32, u64)>);

impl SiteCounts {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn iter(&self) -> std::slice::Iter<'_, (u32, u64)> {
        self.0.iter()
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    /// Append an additive delta, preserving its semantic counts.
    pub fn extend(&mut self, delta: impl IntoIterator<Item = (u32, u64)>) {
        self.0.extend(delta);
    }
    pub fn extend_from_slice(&mut self, delta: &Self) {
        self.0.extend_from_slice(&delta.0);
    }
}
impl FromIterator<(u32, u64)> for SiteCounts {
    fn from_iter<T: IntoIterator<Item = (u32, u64)>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}
impl IntoIterator for SiteCounts {
    type Item = (u32, u64);
    type IntoIter = std::vec::IntoIter<Self::Item>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}
impl<'a> IntoIterator for &'a SiteCounts {
    type Item = &'a (u32, u64);
    type IntoIter = std::slice::Iter<'a, (u32, u64)>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// The common input to the workflow coverage helper. A missing observation
/// means the execution's start was not observed; neither it nor off implies zero.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RanCounts {
    pub observation: Option<RunObservation>,
    /// Statement/block ordinals, followed by action issues and while tests
    /// (`Document::action_ordinals` and `Document::condition_ordinals`).
    pub ran: Vec<(u32, u64)>,
}

/// A verified segment's statement starts and empty-block entries, in order
/// within each task. Tasks retain the order in which they first appeared.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayPath {
    pub tasks: Vec<TaskPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskPath {
    pub task: crate::sites::TaskLabel,
    pub starts: Vec<u32>,
}

/// A recorded semantic path. Task identities and coordinates are interpreted
/// by the kernel's replay reader, rather than by the transporting host.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ExecutionPath(#[doc(hidden)] pub Vec<(crate::sites::TaskLabel, Vec<u32>)>);
impl ExecutionPath {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub fn append(&mut self, delta: Self) {
        for (task, starts) in delta.0 {
            match self.0.last_mut() {
                Some((last, held)) if *last == task => held.extend(starts),
                _ => self.0.push((task, starts)),
            }
        }
    }
    pub fn replay(self) -> ReplayPath {
        let mut tasks: Vec<TaskPath> = Vec::new();
        for (task, starts) in self.0 {
            match tasks.iter_mut().find(|held| held.task == task) {
                Some(held) => held.starts.extend(starts),
                None => tasks.push(TaskPath { task, starts }),
            }
        }
        ReplayPath { tasks }
    }
}
