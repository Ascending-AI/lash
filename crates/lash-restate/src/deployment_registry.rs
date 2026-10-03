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
    async fn unfinished_invocations(
        &self,
        generation: &BuildGeneration,
    ) -> Result<u64, DeploymentRegistryError> {
        let deployments = self.deployments_serving(generation).await?;
        if deployments.is_empty() {
            return Ok(0);
        }
        let counts = self
            .admin
            .open_invocations_by_deployment()
            .await
            .map_err(|error| DeploymentRegistryError {
                detail: format!("read pinned engine invocations: {error}"),
            })?;
        Ok(counts
            .into_iter()
            .filter(|row| {
                row.pinned_deployment_id
                    .as_ref()
                    .is_some_and(|id| deployments.iter().any(|deployment| &deployment.id == id))
            })
            .map(|row| row.open_count)
            .sum())
    }

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

    /// Reads this generation's derived directory, then counts from each
    /// listed group's authoritative record. The service-key equality narrows
    /// Restate's state scanner to one partition key, including all namespaces.
    async fn undrained_group_children(
        &self,
        generation: &BuildGeneration,
    ) -> Result<u64, DeploymentRegistryError> {
        #[derive(serde::Deserialize)]
        struct Entry {
            service_name: String,
            key: String,
            value_utf8: Option<String>,
        }
        let directory = crate::LashService::EffectGroupDrainIndex.base_name();
        let entries: Vec<Entry> = self.admin.query_json(&format!(
            "SELECT service_name, key, value_utf8 FROM state WHERE scope IS NULL AND service_key = {} AND (service_name = {} OR service_name LIKE {}) AND key != '_compat'",
            crate::ingress::sql_string_literal(generation.as_str()),
            crate::ingress::sql_string_literal(directory),
            crate::ingress::sql_string_literal(&format!("%.{directory}")),
        )).await.map_err(|error| DeploymentRegistryError { detail: format!("read the generation's group directory: {error}") })?;
        let mut count = 0_u64;
        for entry in entries {
            let group = entry
                .key
                .strip_prefix(crate::effect_group::drain_index::ENTRY_PREFIX)
                .ok_or_else(|| DeploymentRegistryError {
                    detail: format!("unexpected generation-directory key {}", entry.key),
                })?;
            // Refuse malformed derived state too. A corrupt directory is never
            // retirement evidence that may be treated as empty.
            let raw = parse_state(&entry.service_name, group, entry.value_utf8)?;
            crate::object_state::decode_stamped_value::<()>(
                group,
                raw,
                &crate::effect_group::EFFECT_GROUP_STATE_FORMATS,
            )
            .map_err(|error| DeploymentRegistryError {
                detail: error.to_string(),
            })?;
            let index = entry
                .service_name
                .strip_suffix(directory)
                .map(|prefix| {
                    format!(
                        "{prefix}{}",
                        crate::LashService::EffectGroupState.base_name()
                    )
                })
                .ok_or_else(|| DeploymentRegistryError {
                    detail: "unexpected group-directory service".to_owned(),
                })?;
            let rows: Vec<GroupRow> = self.admin.query_json(&format!(
                "SELECT service_name, service_key, value_utf8 FROM state WHERE scope IS NULL AND service_key = {} AND service_name = {} AND key = {}",
                crate::ingress::sql_string_literal(group),
                crate::ingress::sql_string_literal(&index),
                crate::ingress::sql_string_literal(crate::effect_group::INDEX_RECORD_KEY),
            )).await.map_err(|error| DeploymentRegistryError { detail: format!("read listed group {group}: {error}") })?;
            for row in rows {
                let raw = parse_state(&row.service_name, &row.service_key, row.value_utf8)?;
                count +=
                    crate::effect_group::undrained_children_on(&row.service_key, raw, generation)
                        .map_err(|error| DeploymentRegistryError {
                        detail: error.to_string(),
                    })?;
            }
        }
        Ok(count)
    }
}

#[derive(serde::Deserialize)]
struct GroupRow {
    service_name: String,
    service_key: String,
    value_utf8: Option<String>,
}

fn parse_state(
    service: &str,
    key: &str,
    text: Option<String>,
) -> Result<serde_json::Value, DeploymentRegistryError> {
    let fail = |detail| DeploymentRegistryError {
        detail: format!("effect group {key} in {service}: {detail}"),
    };
    let text = text.ok_or_else(|| fail("its record is not UTF-8".to_owned()))?;
    serde_json::from_str(&text).map_err(|error| fail(format!("its record is not JSON: {error}")))
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
