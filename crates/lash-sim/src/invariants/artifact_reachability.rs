//! Every artifact at session end is reachable from a referrer or collected.
//!
//! ADR 0113: an artifact is kept alive only by its referrers. At the end of a
//! history every stored artifact has a live referrer edge; an artifact with
//! no edge, or only edges of referrers that ended (fenced), should have been
//! collected. A cleanup its ended referrer owes that stalled with a typed
//! reason excuses it, as does one still due in a history that never ran the
//! recovery pass that delivers it: that obligation is the settled-or-stalled
//! checker's.

use super::{History, HistoryChecker, Violation};

pub(super) struct ArtifactReachability;

const INVARIANT: &str = "artifact-reachable-or-collected";

impl HistoryChecker for ArtifactReachability {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        history
            .stores
            .iter()
            .map(|store| store.artifacts.len())
            .sum()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut violations = Vec::new();
        for store in &history.stores {
            let stalled_cleanup = |kind: &str, id: &str| {
                store.cleanups.iter().any(|cleanup| {
                    cleanup.referrer_kind == kind
                        && cleanup.referrer_id == id
                        && (cleanup.state == "stalled"
                            || (cleanup.state == "due" && !history.relay_ran))
                })
            };
            for artifact in &store.artifacts {
                let name = format!("{}/{}", artifact.namespace, artifact.artifact_ref);
                if artifact.referrers.is_empty() {
                    violations.push(
                        Violation::new(
                            INVARIANT,
                            format!(
                                "{}: artifact {name} has no referrer and was not collected",
                                store.label
                            ),
                        )
                        .row(format!("artifact_refs {name}: no referrer edge")),
                    );
                    continue;
                }
                let live = artifact
                    .referrers
                    .iter()
                    .any(|referrer| !store.fences.contains(referrer));
                let excused = artifact
                    .referrers
                    .iter()
                    .any(|(kind, id)| stalled_cleanup(kind, id));
                if !live && !excused {
                    let mut violation = Violation::new(
                        INVARIANT,
                        format!(
                            "{}: artifact {name} is held only by ended referrers and was not collected",
                            store.label
                        ),
                    );
                    for (kind, id) in &artifact.referrers {
                        violation = violation.row(format!(
                            "artifact_referrer_edges {name} <- {kind}:{id} (fenced)"
                        ));
                    }
                    violations.push(violation);
                }
            }
        }
        violations
    }
}
