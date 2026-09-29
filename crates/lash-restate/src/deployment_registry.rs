//! The Restate server's deployments, read as finalize's retirement evidence
//! (ADR 0106 §2, ADR 0115 §3.5 item 7).
//!
//! Retirement is by deployment, not by heartbeat: finalize requires that the
//! server holds no deployment serving the retired generation's lanes. The
//! read is the admin API's deployment listing, the same record Restate routes
//! pinned invocations by, so a deployment that is registered but asleep still
//! counts.

use lash_core::engine::BuildGeneration;
use lash_core_store::store::fleet_finalize::{
    DeploymentRegistry, DeploymentRegistryError, RetainedDeployment,
};

use crate::RestateAdminClient;

/// The deployments a Restate server holds, through its admin API.
#[derive(Clone, Debug)]
pub struct RestateDeploymentRegistry {
    admin: RestateAdminClient,
}

impl RestateDeploymentRegistry {
    /// The registry the admin API at `admin` answers for.
    pub fn new(admin: RestateAdminClient) -> Self {
        Self { admin }
    }
}

#[async_trait::async_trait]
impl DeploymentRegistry for RestateDeploymentRegistry {
    /// Every deployment whose services include a generation lane of
    /// `generation` in any namespace ([`generation_lane_of`]).
    ///
    /// [`generation_lane_of`]: crate::services::generation_lane_of
    async fn deployments_serving(
        &self,
        generation: &BuildGeneration,
    ) -> Result<Vec<RetainedDeployment>, DeploymentRegistryError> {
        let deployments =
            self.admin
                .deployments()
                .await
                .map_err(|error| DeploymentRegistryError {
                    detail: error.to_string(),
                })?;
        Ok(deployments
            .into_iter()
            .filter(|deployment| {
                deployment.services.iter().any(|name| {
                    crate::services::generation_lane_of(name).as_ref() == Some(generation)
                })
            })
            .map(|deployment| RetainedDeployment {
                id: deployment.id,
                uri: deployment.uri,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::services::generation_lane_of;
    use lash_core::engine::BuildGeneration;

    #[test]
    fn a_generation_lane_is_read_in_any_namespace() {
        let generation = BuildGeneration::for_test("registry-old");
        let lane = format!("LashProcessWorkflow_g{}", generation.as_str());
        assert_eq!(generation_lane_of(&lane), Some(generation.clone()));
        assert_eq!(
            generation_lane_of(&format!("tenant-a.{lane}")),
            Some(generation.clone())
        );
        assert_eq!(
            generation_lane_of(&format!("LashTurn_g{}", generation.as_str())),
            Some(generation)
        );
        assert_eq!(generation_lane_of("LashProcessWorkflow"), None);
        assert_eq!(generation_lane_of("tenant-a.LashSession"), None);
        assert_eq!(generation_lane_of("HostService_g0123456789ab"), None);
        assert_eq!(
            generation_lane_of("LashProcessWorkflow_gnot-a-digest"),
            None
        );
    }
}
