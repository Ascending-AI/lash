//! Format sets: which durable formats an actor's state is written in, and
//! which a node decodes (ADR 0106 §1, ADR 0131).
//!
//! A build declares each durable format its actors' state uses as a
//! [`FormatSurface`]: the format's id and the version this build writes. An
//! actor kind's surfaces make up one [`FormatSet`], spelled canonically, so
//! two builds share a set exactly when they declare the same surfaces at the
//! same versions. A node registers every set it decodes; an actor records the
//! set its state is in; a claim takes an actor only when the claiming node
//! decodes its set.

use std::fmt;

use crate::ids::ActorKind;

/// One durable format: its id and the version a build writes.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FormatSurface {
    /// The format's id, e.g. `turn-checkpoint` or `engine/lash_vm`.
    pub id: String,
    /// The version this build writes and reads.
    pub version: u32,
}

impl FormatSurface {
    /// Format `id` at `version`.
    #[must_use]
    pub fn new(id: impl Into<String>, version: u32) -> Self {
        Self {
            id: id.into(),
            version,
        }
    }

    /// The state format of process engine `kind` at `version`.
    #[must_use]
    pub fn engine(kind: &str, version: u32) -> Self {
        Self::new(format!("engine/{kind}"), version)
    }
}

/// The canonical spelling of a set of durable formats. An actor records the
/// set its state is written in; a node declares the sets it decodes, and
/// claims only actors it can decode.
///
/// [`FormatSet::of`] spells a set from its surfaces:
/// `<kind>:<id>@<version>,...`, ids sorted, with `%`, `,` and `@` in an id
/// percent-escaped so no two sets share a spelling. A process that has not
/// started has no state yet: it is in its engine's
/// [`unstarted`](Self::unstarted_process) set, which names the engine only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct FormatSet(String);

impl FormatSet {
    /// A format set as stored.
    #[must_use]
    pub fn new(spelling: impl Into<String>) -> Self {
        Self(spelling.into())
    }

    /// The set of `kind`'s actor state in `surfaces`. A surface named twice
    /// counts once.
    #[must_use]
    pub fn of(kind: ActorKind, surfaces: impl IntoIterator<Item = FormatSurface>) -> Self {
        let mut surfaces: Vec<FormatSurface> = surfaces.into_iter().collect();
        surfaces.sort();
        surfaces.dedup();
        let members: Vec<String> = surfaces
            .iter()
            .map(|surface| format!("{}@{}", escape(&surface.id), surface.version))
            .collect();
        Self(format!("{}:{}", kind.as_str(), members.join(",")))
    }

    /// The set of a process of engine `kind` before its first transition:
    /// it holds no state, so any node with the engine decodes it.
    #[must_use]
    pub fn unstarted_process(kind: &str) -> Self {
        Self(format!("process:unstarted/{}", escape(kind)))
    }

    /// The set of a session actor a producer's first wake created: it holds
    /// no turn yet, so every node that serves sessions decodes it.
    #[must_use]
    pub fn unstarted_session() -> Self {
        Self::new(crate::domain::SESSION_ACTOR_FORMATS)
    }

    /// The set of a kernel process (one with no engine, such as a child
    /// session's turn): it holds no engine state, and every node that serves
    /// processes decodes it.
    #[must_use]
    pub fn kernel_process() -> Self {
        Self("process:kernel".to_owned())
    }

    /// The stored spelling.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for FormatSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

fn escape(id: &str) -> String {
    let mut escaped = String::with_capacity(id.len());
    for character in id.chars() {
        match character {
            '%' => escaped.push_str("%25"),
            ',' => escaped.push_str("%2C"),
            '@' => escaped.push_str("%40"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The set a writer may write, of `candidates` (newest first), given the
/// sets each live node decodes (ADR 0106 §2): the newest candidate that
/// every live node decoding any candidate also decodes, so a newer format is
/// never written while a node that lacks it serves the same actors. A node
/// that decodes none of the candidates serves other actors and does not
/// hold the choice back. With no serving nodes, the newest candidate is
/// unconstrained. With serving nodes but no common candidate, the oldest;
/// `None` only for no candidates.
#[must_use]
pub fn fleet_writable<'a>(
    candidates: &'a [FormatSet],
    live: &[crate::NodeCapability],
) -> Option<&'a FormatSet> {
    let serving: Vec<&Vec<FormatSet>> = live
        .iter()
        .map(|node| &node.decodes)
        .filter(|decodes| candidates.iter().any(|set| decodes.contains(set)))
        .collect();
    candidates
        .iter()
        .find(|set| serving.iter().all(|decodes| decodes.contains(set)))
        .or_else(|| candidates.last())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spelling is canonical: order and repetition of the surfaces do
    /// not change it, a version does, and an id cannot forge a separator.
    #[test]
    fn a_format_set_is_spelled_by_its_surfaces_alone() {
        let one = FormatSet::of(
            ActorKind::Session,
            [
                FormatSurface::new("turn-checkpoint", 1),
                FormatSurface::new("run-record", 1),
            ],
        );
        let again = FormatSet::of(
            ActorKind::Session,
            [
                FormatSurface::new("run-record", 1),
                FormatSurface::new("turn-checkpoint", 1),
                FormatSurface::new("run-record", 1),
            ],
        );
        assert_eq!(one, again);
        assert_eq!(one.as_str(), "session:run-record@1,turn-checkpoint@1");
        let newer = FormatSet::of(
            ActorKind::Session,
            [
                FormatSurface::new("turn-checkpoint", 2),
                FormatSurface::new("run-record", 1),
            ],
        );
        assert_ne!(one, newer);
        let forged = FormatSet::of(ActorKind::Process, [FormatSurface::engine("a@1,b", 1)]);
        let honest = FormatSet::of(
            ActorKind::Process,
            [FormatSurface::engine("a", 1), FormatSurface::new("b", 1)],
        );
        assert_ne!(forged, honest);
        for (literal, separator) in [("a%2Cb", "a,b"), ("a%40b", "a@b")] {
            assert_ne!(
                FormatSet::of(ActorKind::Session, [FormatSurface::new(literal, 1)]),
                FormatSet::of(ActorKind::Session, [FormatSurface::new(separator, 1)]),
            );
        }

        use std::collections::{BTreeMap, BTreeSet};
        let alphabet = ["%", ",", "@", "0", "2", "4", "C", "é"];
        let mut ids = vec![
            String::new(),
            "a%2Cb".into(),
            "a,b".into(),
            "a%40b".into(),
            "a@b".into(),
        ];
        for a in alphabet {
            ids.push(a.into());
            for b in alphabet {
                ids.push(format!("{a}{b}"));
            }
        }
        let surfaces: Vec<_> = ids
            .into_iter()
            .flat_map(|id| [0, 1, u32::MAX].map(|version| FormatSurface::new(id.clone(), version)))
            .collect();
        let mut by_spelling = BTreeMap::new();
        for first in &surfaces {
            for second in &surfaces {
                let members = [first.clone(), second.clone()];
                let normalized: BTreeSet<_> =
                    members.iter().map(|s| (s.id.clone(), s.version)).collect();
                let spelling = FormatSet::of(ActorKind::Session, members.clone());
                let repeated = FormatSet::of(
                    ActorKind::Session,
                    [second.clone(), first.clone(), second.clone()],
                );
                assert_eq!(spelling, repeated);
                if let Some(previous) = by_spelling.insert(spelling, normalized.clone()) {
                    assert_eq!(
                        previous, normalized,
                        "distinct sets must have distinct spellings"
                    );
                }
            }
        }
    }

    #[test]
    fn fleet_choice_obeys_capability_intersection() {
        let candidates: Vec<_> = ["newest", "middle", "oldest"].map(FormatSet::new).into();
        let node = |mask: u8, duplicate| crate::NodeCapability {
            decodes: candidates
                .iter()
                .enumerate()
                .filter(|(at, _)| mask & (1 << at) != 0)
                .flat_map(|(_, set)| {
                    std::iter::repeat_n(set.clone(), if duplicate { 2 } else { 1 })
                })
                .chain([FormatSet::new("unrelated")])
                .collect(),
            ..Default::default()
        };
        assert_eq!(
            fleet_writable(&candidates, &[node(1, false), node(4, false)]),
            Some(&candidates[2])
        );
        assert_eq!(fleet_writable(&[], &[node(7, false)]), None);
        // ADR 0106 §2 chooses the newest candidate every serving node decodes.
        // With none serving there is no reader constraining publication.
        assert_eq!(fleet_writable(&candidates, &[]), Some(&candidates[0]));
        assert_eq!(
            fleet_writable(&candidates, &[node(0, false)]),
            Some(&candidates[0])
        );
        for a in 0..8 {
            for b in 0..8 {
                for c in 0..8 {
                    let masks = [a, b, c];
                    let relevant: Vec<_> = masks.into_iter().filter(|mask| *mask != 0).collect();
                    let common = relevant
                        .iter()
                        .fold(7, |intersection, mask| intersection & mask);
                    let expected = if relevant.is_empty() {
                        0
                    } else {
                        (0..3).find(|at| common & (1 << at) != 0).unwrap_or(2)
                    };
                    for order in [
                        [0, 1, 2],
                        [0, 2, 1],
                        [1, 0, 2],
                        [1, 2, 0],
                        [2, 0, 1],
                        [2, 1, 0],
                    ] {
                        for duplicate in [false, true] {
                            let live = order.map(|at| node(masks[at], duplicate));
                            assert_eq!(
                                fleet_writable(&candidates, &live),
                                Some(&candidates[expected]),
                                "{masks:?}, {order:?}"
                            );
                        }
                    }
                }
            }
        }
    }
}
