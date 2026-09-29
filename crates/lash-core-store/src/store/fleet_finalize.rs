//! Finalize: the operator step that ends a compatibility release's rollback
//! window (ADR 0106 §2, ADR 0115 §2.1 and §3.5).
//!
//! Before finalize the fleet epoch `F` is the older release's, every build of
//! the window writes what that release reads, and the fleet can roll back.
//! Finalize moves `F` to the newer release's epoch in one transaction on the
//! fleet-format row, and from then on the writer fence refuses every writer
//! whose writable range excludes it. It is the last step of the retired
//! generation's drain, and it runs only when:
//!
//! - the retired generation reads drained from the store's own records
//!   ([`GenerationDrainStatus::drained`]), never from heartbeats: a sleeping
//!   deployment can wake after its heartbeat expired;
//! - no deployment the engine still holds serves that generation's lanes
//!   ([`DeploymentRegistry`]), so no pinned journal of the old build can run
//!   again;
//! - no operator hold stands, when the finalize is the automatic one
//!   ([`FinalizeMode`]).
//!
//! A premature finalize is refused typed ([`FinalizeRefusal`]) and changes
//! nothing. The vocabulary lives here so that every store's finalize and the
//! operator binary answer in the same words; the flip itself is the store's.

use crate::build_generation::BuildGeneration;

use super::StoreError;
use super::generation_drain::GenerationDrainStatus;

/// Who is finalizing, which decides whether an operator hold stops it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizeMode {
    /// The drain's own last step, run by the host's rollout: a hold refuses
    /// it with [`FinalizeRefusal::Held`].
    Automatic,
    /// An operator finalizing by hand while the hold stands (ADR 0106 §2:
    /// "with the hold set, the operator runs finalize by hand"). Every other
    /// precondition still holds.
    OverrideHold,
}

/// An operator's hold on the automatic finalize, recorded on the fleet-format
/// row itself so the finalize that reads the row also reads the hold.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FinalizeHold {
    /// Why the operator holds the rollback window open.
    pub reason: String,
    /// Server-clock epoch milliseconds at which the hold was set.
    pub held_at_ms: u64,
}

/// One deployment the engine still holds that serves a generation's lanes.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RetainedDeployment {
    /// The engine's id for the deployment.
    pub id: String,
    /// The endpoint URI it was registered at, when the engine reports one.
    pub uri: Option<String>,
}

/// Why the engine's deployment state could not be read. Finalize fails
/// closed on it: an unread registry is never an empty one.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("the engine's deployment registry could not be read: {detail}")]
pub struct DeploymentRegistryError {
    pub detail: String,
}

/// The engine's record of which deployments are registered: what "the
/// retired generation's deployment is removed" is checked against.
#[async_trait::async_trait]
pub trait DeploymentRegistry: Send + Sync {
    /// Every deployment the engine holds that serves any lane of
    /// `generation`, in any namespace.
    async fn deployments_serving(
        &self,
        generation: &BuildGeneration,
    ) -> Result<Vec<RetainedDeployment>, DeploymentRegistryError>;
}

/// A finalize that must not run yet. Nothing changed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize)]
#[serde(tag = "refusal", rename_all = "snake_case")]
pub enum FinalizeRefusal {
    /// The retired generation still holds work that needs its deployment, or
    /// it was never marked draining.
    #[error(
        "generation {} has not drained: marked draining {}, {} live and {} parked processes, \
         {} parked and {} in-flight turns, {} closing sessions; run `lashctl drain {}` and \
         wait for `lashctl drain-status {}` to read drained",
        status.generation.as_str(),
        status.draining_since_ms.is_some(),
        status.live_processes,
        status.parked_processes,
        status.parked_turns,
        status.in_flight_turns,
        status.closing_sessions,
        status.generation.as_str(),
        status.generation.as_str()
    )]
    GenerationNotDrained { status: GenerationDrainStatus },
    /// The engine still holds a deployment that serves the retired
    /// generation's lanes.
    #[error(
        "the engine still holds {} deployment(s) serving generation {}: {}; remove them once \
         their pinned invocations have drained, then finalize",
        deployments.len(),
        generation.as_str(),
        deployments.iter().map(|deployment| deployment.id.as_str()).collect::<Vec<_>>().join(", ")
    )]
    DeploymentsRetained {
        generation: BuildGeneration,
        deployments: Vec<RetainedDeployment>,
    },
    /// An operator holds the automatic finalize.
    #[error(
        "an operator holds the automatic finalize since {} ms: {}; clear it with \
         `lashctl finalize-hold clear`, or finalize by hand with `--override-hold`",
        hold.held_at_ms,
        hold.reason
    )]
    Held { hold: FinalizeHold },
}

/// What a finalize found and did to `F`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FleetEpochFlip {
    /// `F` moved from `from` to `to` in this call.
    Finalized { from: u32, to: u32 },
    /// `F` already is this build's epoch: an earlier finalize moved it, or
    /// the build opened no rollback window.
    AlreadyFinalized { fleet: u32 },
}

impl FleetEpochFlip {
    /// The epoch the fleet writes after the call.
    pub fn fleet(self) -> u32 {
        match self {
            Self::Finalized { to, .. } => to,
            Self::AlreadyFinalized { fleet } => fleet,
        }
    }
}

/// Why a finalize did not complete.
#[derive(Debug, thiserror::Error)]
pub enum FinalizeError {
    /// A precondition does not hold; nothing changed.
    #[error(transparent)]
    Refused(FinalizeRefusal),
    /// The deployment registry could not be read; nothing changed.
    #[error(transparent)]
    Registry(DeploymentRegistryError),
    /// The store refused or failed: a fenced or incompatible build, or a
    /// storage failure.
    #[error(transparent)]
    Store(StoreError),
}

impl From<StoreError> for FinalizeError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<FinalizeRefusal> for FinalizeError {
    fn from(refusal: FinalizeRefusal) -> Self {
        Self::Refused(refusal)
    }
}

impl From<DeploymentRegistryError> for FinalizeError {
    fn from(error: DeploymentRegistryError) -> Self {
        Self::Registry(error)
    }
}

/// The retirement half of finalize's precondition, shared by every store:
/// `retired` reads drained and no deployment serves it. The hold and the
/// flip are the store's, inside the transaction that moves `F`.
pub async fn require_retired(
    drain: &GenerationDrainStatus,
    registry: &dyn DeploymentRegistry,
) -> Result<(), FinalizeError> {
    if !drain.drained() {
        return Err(FinalizeRefusal::GenerationNotDrained {
            status: drain.clone(),
        }
        .into());
    }
    let deployments = registry.deployments_serving(&drain.generation).await?;
    if !deployments.is_empty() {
        return Err(FinalizeRefusal::DeploymentsRetained {
            generation: drain.generation.clone(),
            deployments,
        }
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Registry(Vec<RetainedDeployment>);

    #[async_trait::async_trait]
    impl DeploymentRegistry for Registry {
        async fn deployments_serving(
            &self,
            _generation: &BuildGeneration,
        ) -> Result<Vec<RetainedDeployment>, DeploymentRegistryError> {
            Ok(self.0.clone())
        }
    }

    fn status(marked: bool, parked_turns: u64) -> GenerationDrainStatus {
        GenerationDrainStatus {
            generation: BuildGeneration::for_test("finalize-old"),
            draining_since_ms: marked.then_some(5),
            live_processes: 0,
            parked_processes: 0,
            parked_turns,
            in_flight_turns: 0,
            closing_sessions: 0,
            stalled_obligations: BTreeMap::new(),
            checked_at: 9,
        }
    }

    #[tokio::test]
    async fn retirement_needs_a_drained_generation_and_no_deployment() {
        let none = Registry(Vec::new());
        let one = Registry(vec![RetainedDeployment {
            id: "dp_old".to_owned(),
            uri: Some("http://old".to_owned()),
        }]);
        for undrained in [status(false, 0), status(true, 1)] {
            assert!(matches!(
                require_retired(&undrained, &none).await,
                Err(FinalizeError::Refused(
                    FinalizeRefusal::GenerationNotDrained { .. }
                ))
            ));
        }
        match require_retired(&status(true, 0), &one).await {
            Err(FinalizeError::Refused(FinalizeRefusal::DeploymentsRetained {
                deployments,
                ..
            })) => assert_eq!(deployments[0].id, "dp_old"),
            other => panic!("a retained deployment must refuse: {other:?}"),
        }
        require_retired(&status(true, 0), &none)
            .await
            .expect("drained and removed");
    }

    #[test]
    fn refusals_serialize_tagged_and_name_their_remedy() {
        let held = FinalizeRefusal::Held {
            hold: FinalizeHold {
                reason: "watch N+1 for a day".to_owned(),
                held_at_ms: 7,
            },
        };
        assert_eq!(
            serde_json::to_value(&held).expect("serialize"),
            serde_json::json!({"refusal":"held","hold":{"reason":"watch N+1 for a day","held_at_ms":7}})
        );
        assert!(held.to_string().contains("lashctl finalize-hold clear"));
        let undrained = FinalizeRefusal::GenerationNotDrained {
            status: status(false, 0),
        };
        assert_eq!(
            serde_json::to_value(&undrained).expect("serialize")["refusal"],
            "generation_not_drained"
        );
        assert!(undrained.to_string().contains("lashctl drain"));
    }
}
