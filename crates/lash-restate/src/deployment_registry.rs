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

    /// Every effect-group index record the server retains, in any
    /// namespace, read through the admin API's `state` table: the committed
    /// children of the groups on a lane of `generation` whose seat is owed
    /// (FIG-4454). A record that does not decode fails the read closed —
    /// an unread group is never a drained one.
    async fn undrained_group_children(
        &self,
        generation: &BuildGeneration,
    ) -> Result<u64, DeploymentRegistryError> {
        #[derive(serde::Deserialize)]
        struct Row {
            service_name: String,
            service_key: String,
            value_utf8: Option<String>,
        }
        let query = format!(
            "SELECT service_name, service_key, value_utf8 FROM state WHERE key = {}",
            crate::ingress::sql_string_literal(crate::effect_group::INDEX_RECORD_KEY)
        );
        let rows: Vec<Row> =
            self.admin
                .query_json(&query)
                .await
                .map_err(|error| DeploymentRegistryError {
                    detail: format!("read the effect-group index records: {error}"),
                })?;
        let index = crate::LashService::EffectGroupState.base_name();
        let mut undrained = 0_u64;
        for row in rows {
            let local = row
                .service_name
                .rsplit_once('.')
                .map_or(row.service_name.as_str(), |(_, local)| local);
            if local != index {
                continue;
            }
            let unreadable = |detail: String| DeploymentRegistryError {
                detail: format!(
                    "effect group {} in {}: {detail}",
                    row.service_key, row.service_name
                ),
            };
            let raw = row
                .value_utf8
                .as_deref()
                .ok_or_else(|| unreadable("its index record is not UTF-8".to_owned()))
                .and_then(|text| {
                    serde_json::from_str::<serde_json::Value>(text).map_err(|error| {
                        unreadable(format!("its index record is not JSON: {error}"))
                    })
                })?;
            undrained +=
                crate::effect_group::undrained_children_on(&row.service_key, raw, generation)
                    .map_err(|error| unreadable(error.to_string()))?;
        }
        Ok(undrained)
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
