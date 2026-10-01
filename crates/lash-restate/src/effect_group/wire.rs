//! Wire shapes shared between the effect-group objects and their callers:
//! serde adapters, phase projection, and the admission-fence request.

use serde::{Deserialize, Serialize};

/// Serializes a `BTreeMap` as ordered key-value pairs, so a persisted map's
/// wire form does not depend on a serializer's object ordering.
pub(crate) mod btree_map_as_pairs {
    use std::collections::BTreeMap;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<K, V, S>(map: &BTreeMap<K, V>, serializer: S) -> Result<S::Ok, S::Error>
    where
        K: Ord + Serialize,
        V: Serialize,
        S: Serializer,
    {
        map.iter().collect::<Vec<_>>().serialize(serializer)
    }

    pub fn deserialize<'de, K, V, D>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
    where
        K: Ord + Deserialize<'de>,
        V: Deserialize<'de>,
        D: Deserializer<'de>,
    {
        Vec::<(K, V)>::deserialize(deserializer).map(|pairs| pairs.into_iter().collect())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupPhase {
    Preparing,
    Ready,
    Closed,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupProbeResponse {
    Absent,
    Exists {
        shape_digest: String,
        phase: EffectGroupPhase,
    },
}

/// One paused dispatcher invocation of a group, as the park reconcile names
/// it to the index (FIG-4630).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct EffectGroupOpenerRequest {
    /// The dispatcher handler the invocation runs: `run`, `child` or
    /// `retire`.
    pub handler: String,
    /// The invocation's id: a child's position is the one the index retains
    /// this id for.
    pub invocation_id: String,
}

/// What a group still needs of one paused dispatcher invocation, as the park
/// reconcile asks (FIG-4607, FIG-4630).
///
/// Whether the opener still awaits a child and whether anything still needs
/// it are two facts. A closed group's opener consumes no more ranks, but a
/// `RunToCompletion` loser still has to settle before its scope is quiescent,
/// and a committed child still owes its drain: both are needed. A child whose
/// position is seated owes nothing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub(crate) enum EffectGroupOpenerResponse {
    /// The group still needs the invocation: a child whose position is not
    /// seated, the dispatch of a group not retired, or a retirement not
    /// answered. It parks `opener`, the scope the work runs for.
    Needed {
        #[serde(with = "lash_core::admitted_scope_wire")]
        opener: lash_core::AdmittedScope,
    },
    /// Nothing needs the invocation: its position is seated (a cancel
    /// decision or its own settlement took the seat), or its group is
    /// retired or was never opened. It is released.
    Seated,
}

/// A semantic admission under one recorded group child (ADR 0099 §4,
/// FIG-3470): the index-side answer the SQL claim's minting-row fence and
/// the native group mutex give on their tiers. A child-bound controller asks
/// it before every effect's `ctx.run`, so the admission and the decision
/// that would refuse it serialize on the same index object.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectGroupAdmitSemanticRequest {
    /// The bound child's declared replay key; the index resolves its
    /// position from the retained shape rather than trusting a
    /// caller-supplied position.
    pub replay_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EffectGroupAdmitSemanticResponse {
    /// The child holds no committed cancel decision: the admission proceeds.
    /// A `Committed`-but-unsettled child admits too — it retains authority to
    /// finish its drain (§4).
    Admitted,
    /// The child's cancel disposition committed; the admission refuses.
    CancelDecided,
    /// The replay key is not a retained member of this group.
    UnknownChild,
    /// No live index record holds this group key, including a retired one:
    /// a reaped or retired group has no live state to arbitrate under.
    UnknownGroup,
}
