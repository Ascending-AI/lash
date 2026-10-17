//! Selects the current helpers only when the serving fleet can read them.

#[cfg(feature = "rlm")]
use lash_core::durable_port::FormatSet;

/// The helper release a node of `backend`'s build writes a cell against:
/// its own, once every live node that serves the sessions of a build of an
/// earlier release it retains also decodes its sets; until then, the
/// newest such release they all hold (ADR 0106 §2).
#[cfg(feature = "rlm")]
pub(crate) struct FleetHelperWrites {
    backend: crate::Backend,
}

#[cfg(feature = "rlm")]
impl FleetHelperWrites {
    pub(crate) fn new(backend: crate::Backend) -> Self {
        Self { backend }
    }
}

#[cfg(feature = "rlm")]
fn fleet_helper_release(
    candidates: &[(u32, FormatSet)],
    carried: &[FormatSet],
    live: &[Vec<FormatSet>],
) -> Option<u32> {
    // A carried turn can reach its first new cell without a snapshot. Nodes
    // serving those sets participate even if they decode no lowering candidate.
    let serving: Vec<_> = live
        .iter()
        .filter(|decodes| {
            candidates.iter().any(|(_, set)| decodes.contains(set))
                || carried.iter().any(|set| decodes.contains(set))
        })
        .collect();
    candidates
        .iter()
        .find(|(_, set)| serving.iter().all(|decodes| decodes.contains(set)))
        .map(|(release, _)| *release)
}

#[cfg(feature = "rlm")]
fn helpers_unavailable(message: impl Into<String>) -> lash_core::RuntimeEffectControllerError {
    lash_core::RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::RunDefinitionUnavailable,
        message,
    )
    .retryable_uncommitted_derivation()
}

#[cfg(feature = "rlm")]
#[async_trait::async_trait]
impl lash_protocol_rlm::HelperReleaseGate for FleetHelperWrites {
    async fn writable(&self) -> Result<u32, lash_core::RuntimeEffectControllerError> {
        let formats = self.backend.formats();
        let own = crate::formats::helper_release_of(self.backend.surfaces());
        let mut candidates = vec![(own, formats.session().clone())];
        candidates.extend(
            formats
                .decoded_sessions()
                .into_iter()
                .map(|(surfaces, set)| (crate::formats::helper_release_of(surfaces), set)),
        );
        let live = self
            .backend
            .durable()
            .live_decodes()
            .await
            .map_err(|error| {
                tracing::warn!(?candidates, carried = ?formats.carried_sessions(), ?error, "helper fleet survey unavailable; lowering deferred");
                helpers_unavailable(format!("helper fleet survey unavailable: {error}"))
            })?;
        let chosen = fleet_helper_release(&candidates, formats.carried_sessions(), &live);
        tracing::info!(?candidates, carried = ?formats.carried_sessions(), ?live, ?chosen, "helper release fleet gate");
        chosen.ok_or_else(|| {
            helpers_unavailable(
                "no helper release is readable by every live node serving these sessions",
            )
        })
    }
}

#[cfg(all(test, feature = "rlm"))]
mod tests {
    use super::*;

    /// V19: an old-only node still co-serves a carried turn, even when it decodes no lowering candidate.
    #[test]
    fn a_carried_turn_defers_helpers_no_co_serving_node_can_read() {
        let new = FormatSet::new("session:kernel-parked-state@2,kernel-helpers@2");
        let old = FormatSet::new("session:kernel-parked-state@1");
        let live = vec![vec![new.clone(), old.clone()], vec![old.clone()]];
        assert_eq!(fleet_helper_release(&[(2, new)], &[old], &live), None);
    }
}
