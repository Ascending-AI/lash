//! The identities of tasks and effects within a run (`K-TASK-020`,
//! `K-EFF-008`). They are derived from the document's sites, so a host can
//! tell which element of a fan-out a task or an effect belongs to.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{FunctionId, Name};

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

/// An engine-owned task label. Its ordering is opaque and engine-defined;
/// embeddings may store, compare and display it, never interpret its grammar.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TaskLabel(#[doc(hidden)] pub TaskIdentity);

/// An engine-owned position, comparable with a document's position labels.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PositionLabel(#[doc(hidden)] pub Site);

/// One visit to a position by a task. The order is engine-defined and has no
/// numeric accessor. Counts belong to the semantic observation reader.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VisitId(#[doc(hidden)] pub u64);

/// An effect's stable, opaque identity within its program execution.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EffectId(#[doc(hidden)] pub EffectIdentity);

impl EffectId {
    pub fn position(&self) -> PositionLabel {
        PositionLabel(self.0.site.clone())
    }
    pub fn task(&self) -> TaskLabel {
        TaskLabel(self.0.task.clone())
    }
    pub fn visit(&self) -> VisitId {
        VisitId(self.0.occurrence)
    }
}
impl Site {
    pub fn position(&self) -> PositionLabel {
        PositionLabel(self.clone())
    }
}
impl PositionLabel {
    /// The document site this position names: the key a host looks the
    /// position up by in a workflow view.
    pub fn site(&self) -> &Site {
        &self.0
    }
    /// The callee's display label, when this position is in a named function.
    pub fn callee_label(&self) -> Option<&str> {
        match &self.0.unit {
            Unit::Function(name) => Some(name.as_str()),
            _ => None,
        }
    }
}
impl std::fmt::Display for PositionLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::fmt::Display for VisitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
impl std::fmt::Display for TaskLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            TaskIdentity::Main => f.write_str("main"),
            TaskIdentity::Spawned(spawn) => {
                write!(f, "the task of {}#{}", spawn.site, spawn.occurrence)
            }
        }
    }
}
impl std::fmt::Display for EffectId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}#{} in {}", self.position(), self.visit(), self.task())
    }
}
macro_rules! opaque_codec {
    ($opaque:ident, $inner:ty) => {
        impl crate::tokens::TokenCodec for $opaque {
            fn encode(&self, out: &mut crate::tokens::TokenWriter) {
                self.0.encode(out);
            }
            fn decode(
                input: &mut crate::tokens::TokenReader<'_>,
            ) -> Result<Self, crate::tokens::TokenError> {
                <$inner as crate::tokens::TokenCodec>::decode(input).map(Self)
            }
        }
    };
}
opaque_codec!(EffectId, EffectIdentity);
opaque_codec!(TaskLabel, TaskIdentity);
opaque_codec!(PositionLabel, Site);
opaque_codec!(VisitId, u64);

/// Exact semantic observations of one task's position, without its coordinate
/// grammar. Zero-based loop counters are reported as counts of loop entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PositionCounts {
    pub starts: u64,
    pub loops: Vec<LoopCounts>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoopCounts {
    pub position: PositionLabel,
    pub entries: u64,
}

/// Kernel-owned folding of duplicate or reordered live position frames.
/// Derived state only: never changes execution, charge or persistence.
#[derive(Clone, Debug, Default)]
pub struct ObservationReader {
    live: std::collections::BTreeMap<(TaskLabel, PositionLabel), EffectId>,
}
impl ObservationReader {
    pub fn clear(&mut self) {
        self.live.clear();
    }
    /// Returns only new visits, in the engine's opaque order. A duplicate or
    /// an older frame never moves a task's displayed position backwards.
    pub fn fold(&mut self, frame: impl IntoIterator<Item = EffectId>) -> Vec<EffectId> {
        let mut accepted = Vec::new();
        for at in frame {
            let key = (at.task(), at.position());
            if self
                .live
                .get(&key)
                .is_some_and(|held| held.0.occurrence >= at.0.occurrence)
            {
                continue;
            }
            self.live.insert(key, at.clone());
            accepted.push(at);
        }
        accepted.sort();
        accepted
    }
    pub fn counts(at: &EffectId) -> PositionCounts {
        PositionCounts {
            starts: at.0.occurrence.saturating_add(1),
            loops: at
                .0
                .loops
                .iter()
                .map(|loop_| LoopCounts {
                    position: loop_.site.position(),
                    entries: loop_.iteration.saturating_add(1),
                })
                .collect(),
        }
    }
    /// Turn-level control requests are legal only on the root task.
    pub fn is_root_task(at: &EffectId) -> bool {
        matches!(at.0.task, TaskIdentity::Main)
    }
}

impl ObservationReader {
    pub fn task_is_root(task: &TaskLabel) -> bool {
        matches!(task.0, TaskIdentity::Main)
    }
    pub fn task_position_label(task: &TaskLabel, position: &PositionLabel) -> String {
        match &task.0 {
            TaskIdentity::Main => position.to_string(),
            TaskIdentity::Spawned(spawn) => format!(
                "{position} in the task of {}#{}",
                spawn.site, spawn.occurrence
            ),
        }
    }
    pub fn task_label(task: &TaskLabel) -> String {
        match &task.0 {
            TaskIdentity::Main => "main".to_owned(),
            TaskIdentity::Spawned(spawn) => {
                let unit = match &spawn.site.unit {
                    Unit::Main => "main".to_owned(),
                    Unit::Function(name) => format!("fn:{name}"),
                    Unit::Library(function) => format!("lib:{function}"),
                };
                let path = spawn
                    .site
                    .path
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(".");
                format!("{unit}/{path}#{}", spawn.occurrence)
            }
        }
    }
    pub fn task_source<M: SourceLocations + ?Sized>(
        task: &TaskLabel,
        map: &M,
        body: Option<&PositionLabel>,
    ) -> Option<usize> {
        let TaskIdentity::Spawned(spawn) = &task.0 else {
            return None;
        };
        map.source_start(&spawn.site)
            .or_else(|| body.and_then(|body| map.source_start(&body.0)))
    }
}
/// Runtime source-map projection, implemented beside the document/source map.
pub trait SourceLocations {
    fn source_start(&self, site: &Site) -> Option<usize>;
}

impl From<Site> for PositionLabel {
    fn from(site: Site) -> Self {
        Self(site)
    }
}
impl PartialEq<Site> for PositionLabel {
    fn eq(&self, site: &Site) -> bool {
        self.0 == *site
    }
}
impl PartialEq<PositionLabel> for Site {
    fn eq(&self, site: &PositionLabel) -> bool {
        *self == site.0
    }
}

// JSON carries a typed opaque token. Binary graph codecs continue to encode
// the engine value directly, so checkpoint and admission bytes are unchanged.
macro_rules! opaque_json {
    ($opaque:ident, $inner:ty, $prefix:literal, $pattern:literal) => {
        impl Serialize for $opaque {
            fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                let bytes = crate::tokens::encode_body(&self.0);
                let mut token = String::from($prefix);
                crate::legacy_names::write_hex(&bytes, &mut token);
                serializer.serialize_str(&token)
            }
        }
        impl<'de> Deserialize<'de> for $opaque {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let token = String::deserialize(deserializer)?;
                let bytes = token
                    .strip_prefix($prefix)
                    .and_then(crate::legacy_names::parse_hex)
                    .ok_or_else(|| {
                        serde::de::Error::custom(concat!("invalid ", stringify!($opaque), " token"))
                    })?;
                crate::tokens::decode_body::<$inner>(&bytes)
                    .map(Self)
                    .map_err(serde::de::Error::custom)
            }
        }
        crate::legacy_names::string_schema!($opaque, stringify!($opaque), $pattern);
    };
}
opaque_json!(TaskLabel, TaskIdentity, "t:", "^t:([0-9a-f]{2})+$");
opaque_json!(PositionLabel, Site, "p:", "^p:([0-9a-f]{2})+$");
opaque_json!(VisitId, u64, "v:", "^v:([0-9a-f]{2})+$");
opaque_json!(EffectId, EffectIdentity, "e:", "^e:([0-9a-f]{2})+$");

/// An opaque delivery address within one running execution. The engine owns
/// its stable encoding and order; hosts only retain it and return it on delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WaitId(#[doc(hidden)] pub u64);
impl std::fmt::Display for WaitId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}
opaque_codec!(WaitId, u64);
opaque_json!(WaitId, u64, "w:", "^w:([0-9a-f]{2})+$");

/// The code a node belongs to.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Unit {
    /// The document's `main`.
    Main,
    /// A declared function of the document.
    Function(Name),
    /// The kernel-code body of a library function.
    Library(FunctionId),
}

/// A node's address: the code it belongs to and the chain of
/// [`Node::children`] indexes that reaches it from that code's body.
///
/// A site is derived from the document and is never written by a host
/// (`K-ID-004`). An empty chain addresses the body itself.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct Site {
    pub unit: Unit,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub path: Vec<u32>,
}

impl Site {
    pub fn new(unit: Unit, path: impl Into<Vec<u32>>) -> Self {
        Self {
            unit,
            path: path.into(),
        }
    }

    /// The site of this node's `index`-th child.
    pub fn child(&self, index: u32) -> Self {
        let mut path = self.path.clone();
        path.push(index);
        Self {
            unit: self.unit.clone(),
            path,
        }
    }
}

impl std::fmt::Display for Site {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.unit {
            Unit::Main => f.write_str("main")?,
            Unit::Function(name) => write!(f, "fn {name}")?,
            Unit::Library(id) => write!(f, "function {id}")?,
        }
        for step in &self.path {
            write!(f, "/{step}")?;
        }
        Ok(())
    }
}

/// Engine-owned fixtures for host laws of identity equality and transport.
pub mod testkit {
    /// Delivery-address fixtures for host laws of matching and withdrawn waits.
    pub const fn wait_id(ordinal: u64) -> crate::sites::WaitId {
        crate::sites::WaitId(ordinal)
    }

    pub fn root_task() -> crate::sites::TaskLabel {
        crate::sites::TaskLabel(crate::sites::TaskIdentity::Main)
    }
    pub fn spawned_task(
        parent: crate::sites::TaskLabel,
        site: impl Into<crate::sites::PositionLabel>,
        occurrence: u64,
    ) -> crate::sites::TaskLabel {
        crate::sites::TaskLabel(crate::sites::TaskIdentity::Spawned(
            crate::sites::SpawnIdentity {
                parent: std::sync::Arc::new(parent.0),
                site: site.into().0,
                occurrence,
            },
        ))
    }
    pub const fn visit(ordinal: u64) -> crate::sites::VisitId {
        crate::sites::VisitId(ordinal)
    }
    pub fn effect(
        task: crate::sites::TaskLabel,
        site: impl Into<crate::sites::PositionLabel>,
        occurrence: u64,
        loops: Vec<(crate::sites::Site, u64)>,
    ) -> crate::sites::EffectId {
        crate::sites::EffectId(crate::sites::EffectIdentity {
            task: task.0,
            site: site.into().0,
            occurrence,
            loops: loops
                .into_iter()
                .map(|(site, iteration)| crate::sites::LoopIteration { site, iteration })
                .collect(),
        })
    }
    /// Repeated-visit fixtures for the semantic live-frame folding laws.
    pub fn with_visit(id: &crate::sites::EffectId, visit: u64) -> crate::sites::EffectId {
        let mut raw = id.0.clone();
        raw.occurrence = visit;
        crate::sites::EffectId(raw)
    }
    /// Position fixtures for document-binding and mismatch laws.
    pub fn with_position(
        id: &crate::sites::EffectId,
        position: impl Into<crate::sites::PositionLabel>,
    ) -> crate::sites::EffectId {
        let mut raw = id.0.clone();
        raw.site = position.into().0;
        crate::sites::EffectId(raw)
    }
    /// Fan-out fixtures for host observation attribution laws.
    pub fn with_task(
        id: &crate::sites::EffectId,
        task: crate::sites::TaskLabel,
    ) -> crate::sites::EffectId {
        let mut raw = id.0.clone();
        raw.task = task.0;
        crate::sites::EffectId(raw)
    }
    /// Enclosing-loop fixtures for the host observation transport laws.
    pub fn with_loops(
        id: &crate::sites::EffectId,
        loops: Vec<(crate::sites::Site, u64)>,
    ) -> crate::sites::EffectId {
        let mut raw = id.0.clone();
        raw.loops = loops
            .into_iter()
            .map(|(site, iteration)| crate::sites::LoopIteration { site, iteration })
            .collect();
        crate::sites::EffectId(raw)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::{TokenCodec, encode_body};
    use serde::Serialize;
    use serde::de::DeserializeOwned;

    /// FIG-6711: host JSON carries typed opaque tokens, never engine records;
    /// the binary checkpoint/admission identity encoding is unchanged.
    #[test]
    fn neutral_occurrence_tokens_preserve_binary_identity() {
        fn token<T: TokenCodec + Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(
            value: T,
            baseline: Vec<u8>,
            wrong_kind: serde_json::Value,
        ) {
            let json = serde_json::to_value(&value).unwrap();
            assert!(json.is_string());
            assert_eq!(serde_json::from_value::<T>(json).unwrap(), value);
            assert!(serde_json::from_value::<T>(wrong_kind).is_err());
            assert_eq!(encode_body(&value), baseline);
        }
        let task = TaskIdentity::Main;
        let site = Site::new(crate::sites::Unit::Main, vec![2]);
        let effect = EffectIdentity {
            task: task.clone(),
            site: site.clone(),
            occurrence: 3,
            loops: vec![],
        };
        token(
            TaskLabel(task.clone()),
            encode_body(&task),
            serde_json::json!("w:0300"),
        );
        token(
            PositionLabel(site.clone()),
            encode_body(&site),
            serde_json::json!({ "unit": "main", "path": [2] }),
        );
        token(
            EffectId(effect.clone()),
            encode_body(&effect),
            serde_json::to_value(&effect).unwrap(),
        );
        token(VisitId(3), encode_body(&3_u64), serde_json::json!(3));
        token(WaitId(3), encode_body(&3_u64), serde_json::json!(3));
    }
}
