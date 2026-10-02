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
    /// The state-table stand-in supplies a full scan or an exact partition-key
    /// read. It excludes live server scheduling and measures the operator's
    /// unchanged decode/count path with identical retained records.
    #[derive(Debug)]
    struct BenchmarkState {
        generation: String,
        full: bytes::Bytes,
        directory: bytes::Bytes,
        groups: std::collections::BTreeMap<String, bytes::Bytes>,
        rows: std::sync::atomic::AtomicUsize,
        bytes: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl lash_http_transport::HttpTransport for BenchmarkState {
        async fn send(
            &self,
            request: lash_http_transport::HttpRequest,
            _timeout: Option<std::time::Duration>,
        ) -> Result<lash_http_transport::HttpResponse, lash_http_transport::LlmTransportError>
        {
            let request: serde_json::Value =
                serde_json::from_slice(&request.body).expect("SQL request");
            let query = request["query"].as_str().expect("SQL");
            let (body, rows) = if let Some(tail) = query.split("service_key = '").nth(1) {
                assert!(
                    query.contains("scope IS NULL"),
                    "point reads must select the unscoped partition key"
                );
                let key = tail.split('\'').next().expect("key");
                if key == self.generation {
                    (self.directory.clone(), 34)
                } else {
                    (
                        self.groups
                            .get(key)
                            .expect("only a listed group may be fetched")
                            .clone(),
                        1,
                    )
                }
            } else {
                (self.full.clone(), self.groups.len())
            };
            self.rows
                .fetch_add(rows, std::sync::atomic::Ordering::Relaxed);
            self.bytes
                .fetch_add(body.len(), std::sync::atomic::Ordering::Relaxed);
            Ok(lash_http_transport::HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: lash_http_transport::HttpResponseBody::buffered(body),
            })
        }
    }

    fn benchmark_state(population: usize, generation: &BuildGeneration) -> BenchmarkState {
        use crate::effect_group::*;
        use std::collections::BTreeMap;
        let mut rows = Vec::new();
        let mut groups = BTreeMap::new();
        let other = BuildGeneration::for_test("benchmark-other");
        for ordinal in 0..population + 34 {
            let key = format!("bench-{ordinal}");
            let drained = (2..34).contains(&ordinal);
            let route = if ordinal >= 2 && !drained {
                &other
            } else {
                generation
            };
            let record = EffectGroupStateRecord {
                shape_digest: "benchmark".to_owned(),
                dispatch_route: format!("EffectGroupDispatch_g{route}"),
                lifecycle: EffectGroupLifecycle::Ready {
                    addresses: BTreeMap::from([(0, "inv-benchmark".to_owned())]),
                    live: EffectGroupStateLiveRecord {
                        shape: EffectGroupShape {
                            wake: lash_core::GroupWakePolicy::All,
                            loser_disposition: lash_core::LoserPolicy::RunToCompletion,
                            replay_keys: vec![format!("{key}-child")],
                            opener: lash_core::AdmittedScope::turn("session", "turn"),
                        },
                        decisions: vec![EffectGroupDecision {
                            position: 0,
                            seat: if drained {
                                EffectGroupSeat::Seated {
                                    terminal: EffectGroupSettlementTerminal::Cancelled,
                                }
                            } else {
                                EffectGroupSeat::Committed
                            },
                        }],
                    },
                },
            };
            let raw =
                serde_json::json!({ "format": EFFECT_GROUP_STATE_FORMAT_VERSION, "body": record });
            let row = serde_json::json!({"service_name": "EffectGroupIndex", "service_key": key, "value_utf8": raw.to_string()});
            groups.insert(
                key,
                bytes::Bytes::from(
                    serde_json::to_vec(&serde_json::json!({"rows": [row.clone()]}))
                        .expect("point response"),
                ),
            );
            rows.push(row);
        }
        let directory = (0..34).map(|ordinal| serde_json::json!({
            "service_name": "EffectGroupDrainIndex", "key": format!("group/bench-{ordinal}"),
            "value_utf8": serde_json::json!({"format": EFFECT_GROUP_STATE_FORMAT_VERSION, "body": null}).to_string(),
        })).collect::<Vec<_>>();
        BenchmarkState {
            generation: generation.to_string(),
            full: serde_json::to_vec(&serde_json::json!({"rows": rows}))
                .expect("scan response")
                .into(),
            directory: serde_json::to_vec(&serde_json::json!({"rows": directory}))
                .expect("directory response")
                .into(),
            groups,
            rows: Default::default(),
            bytes: Default::default(),
        }
    }

    async fn original_scan(admin: &crate::RestateAdminClient, generation: &BuildGeneration) -> u64 {
        let rows: Vec<super::GroupRow> = admin.query_json(
            "SELECT service_name, service_key, value_utf8 FROM state WHERE key = 'effect-group/v1/state'",
        ).await.expect("original query");
        rows.into_iter()
            .map(|row| {
                crate::effect_group::undrained_children_on(
                    &row.service_key,
                    super::parse_state(&row.service_name, &row.service_key, row.value_utf8)
                        .expect("record JSON"),
                    generation,
                )
                .expect("original count")
            })
            .sum()
    }

    #[tokio::test]
    #[ignore = "interleaved retirement-poll benchmark; run explicitly on the measurement host"]
    async fn retirement_poll_benchmark() {
        use lash_core_store::store::fleet_finalize::DeploymentRegistry as _;
        use std::sync::atomic::Ordering;
        let generation = BuildGeneration::for_test("retirement-benchmark");
        for population in [0, 1_000, 10_000] {
            let transport = std::sync::Arc::new(benchmark_state(population, &generation));
            let admin = crate::RestateAdminClient::new(crate::RestateConnection::with_transport(
                "https://restate.invalid",
                transport.clone(),
            ));
            let registry = super::RestateDeploymentRegistry::new(admin.clone());
            let mut before = Vec::new();
            let mut after = Vec::new();
            for round in 0..7 {
                for optimized in if round % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                } {
                    transport.rows.store(0, Ordering::Relaxed);
                    transport.bytes.store(0, Ordering::Relaxed);
                    let start = std::time::Instant::now();
                    let count = if optimized {
                        registry
                            .undrained_group_children(&generation)
                            .await
                            .expect("indexed poll")
                    } else {
                        original_scan(&admin, &generation).await
                    };
                    let elapsed = start.elapsed().as_nanos();
                    assert_eq!(count, 2);
                    let rows = transport.rows.load(Ordering::Relaxed);
                    assert_eq!(rows, if optimized { 68 } else { population + 34 });
                    println!(
                        "retirement-poll population={population} side={} round={} ns={elapsed} rows={rows} bytes={}",
                        if optimized { "after" } else { "before" },
                        round + 1,
                        transport.bytes.load(Ordering::Relaxed)
                    );
                    if optimized {
                        after.push(elapsed);
                    } else {
                        before.push(elapsed);
                    }
                }
            }
            println!(
                "retirement-poll population={population} before_ns={before:?} after_ns={after:?}"
            );
            before.sort_unstable();
            after.sort_unstable();
            println!(
                "retirement-poll population={population} median_before_ns={} median_after_ns={} groups_before={} groups_after=34",
                before[3],
                after[3],
                population + 34
            );
        }
    }
}
