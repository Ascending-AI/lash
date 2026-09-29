//! What a leg reads from the live `restate-server`: object state, the
//! invocations of a handler, and the deployments, through Restate's SQL
//! introspection and its admin API. Introspection measures; it never fences
//! (ADR 0115 §3.2).

use std::collections::BTreeMap;

use anyhow::{Context, Result, anyhow, bail};
use lash_restate::{ObjectCompat, RestateAdminClient, RestateConnection, RestateNamespace};
use serde::Deserialize;

/// One deployment the server holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Deployment {
    pub id: String,
    /// The URI it was registered at.
    pub endpoint: String,
}

/// One invocation of a handler, as `sys_invocation` reports it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Invocation {
    pub id: String,
    pub status: String,
    #[serde(default)]
    pub pinned_deployment_id: Option<String>,
    #[serde(default)]
    pub invoked_by_id: Option<String>,
    #[serde(default)]
    pub last_failure: Option<String>,
    #[serde(default)]
    pub retry_count: Option<u64>,
}

/// One segment `run` of a process.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct ProcessSegment {
    pub lane: String,
    pub key: String,
    #[serde(flatten)]
    pub invocation: Invocation,
}

/// The server a roll runs against, seen through one ADR 0111 namespace.
pub struct RestateView {
    admin: RestateAdminClient,
    admin_url: String,
    namespace: RestateNamespace,
    http: reqwest::Client,
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

impl RestateView {
    pub fn new(admin_url: &str, namespace: &str) -> Result<Self> {
        Ok(Self {
            admin: RestateAdminClient::new(RestateConnection::new(admin_url.to_owned())),
            admin_url: admin_url.trim_end_matches('/').to_owned(),
            namespace: RestateNamespace::new(namespace)
                .map_err(|error| anyhow!("namespace: {error}"))?,
            http: reqwest::Client::new(),
        })
    }

    /// The Restate name of lash's `service` in this namespace.
    pub fn service_name(&self, service: &str) -> String {
        self.namespace.service_name(service)
    }

    async fn query<T: serde::de::DeserializeOwned>(&self, sql: &str) -> Result<Vec<T>> {
        self.admin
            .query_json(sql)
            .await
            .map_err(|error| anyhow!("Restate SQL `{sql}`: {error}"))
    }

    /// Every value one object holds, by state key, as the JSON it stores.
    pub async fn object_state(
        &self,
        service: &str,
        key: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>> {
        #[derive(Deserialize)]
        struct Row {
            key: String,
            value_utf8: Option<String>,
        }
        let rows: Vec<Row> = self
            .query(&format!(
                "SELECT key, value_utf8 FROM state WHERE service_name = {} AND service_key = {}",
                sql_literal(&self.service_name(service)),
                sql_literal(key)
            ))
            .await?;
        rows.into_iter()
            .map(|row| {
                let text = row
                    .value_utf8
                    .ok_or_else(|| anyhow!("{service}/{key} state `{}` is not UTF-8", row.key))?;
                let value = serde_json::from_str(&text)
                    .with_context(|| format!("{service}/{key} state `{}`: {text}", row.key))?;
                Ok((row.key, value))
            })
            .collect()
    }

    /// The value every object of `service` holds under `state_key`, by
    /// object key.
    pub async fn values_named(
        &self,
        service: &str,
        state_key: &str,
    ) -> Result<BTreeMap<String, serde_json::Value>> {
        #[derive(Deserialize)]
        struct Row {
            service_key: String,
            value_utf8: Option<String>,
        }
        let rows: Vec<Row> = self
            .query(&format!(
                "SELECT service_key, value_utf8 FROM state WHERE service_name = {} AND key = {}",
                sql_literal(&self.service_name(service)),
                sql_literal(state_key)
            ))
            .await?;
        rows.into_iter()
            .map(|row| {
                let text = row.value_utf8.ok_or_else(|| {
                    anyhow!("{service}/{} `{state_key}` is not UTF-8", row.service_key)
                })?;
                let value = serde_json::from_str(&text).with_context(|| {
                    format!("{service}/{} `{state_key}`: {text}", row.service_key)
                })?;
                Ok((row.service_key, value))
            })
            .collect()
    }

    /// Every object of `service` that holds a `_compat` record, with it.
    pub async fn compat_records(&self, service: &str) -> Result<BTreeMap<String, ObjectCompat>> {
        self.values_named(service, lash_restate::COMPAT_KEY)
            .await?
            .into_iter()
            .map(|(key, value)| {
                let compat = serde_json::from_value(value.clone())
                    .with_context(|| format!("{service}/{key} `_compat`: {value}"))?;
                Ok((key, compat))
            })
            .collect()
    }

    /// The keys of every object of `service` whose `_compat` still names
    /// `format`: the synthetic preflight of a sweep.
    pub async fn objects_at_format(&self, service: &str, format: u32) -> Result<Vec<String>> {
        Ok(self
            .compat_records(service)
            .await?
            .into_iter()
            .filter(|(_, compat)| compat.format == format)
            .map(|(key, _)| key)
            .collect())
    }

    /// Every invocation of `service`'s `handler` on the object or workflow
    /// `key`, oldest first.
    pub async fn invocations(
        &self,
        service: &str,
        key: &str,
        handler: &str,
    ) -> Result<Vec<Invocation>> {
        self.query(&format!(
            "SELECT id, status, pinned_deployment_id, invoked_by_id, last_failure, retry_count \
             FROM sys_invocation WHERE target_service_name = {} AND target_service_key = {} \
             AND target_handler_name = {} ORDER BY created_at",
            sql_literal(&self.service_name(service)),
            sql_literal(key),
            sql_literal(handler)
        ))
        .await
    }

    /// Every invocation of any of `service`'s handlers on `key` that has not
    /// completed, oldest first: work a node still owes the object.
    pub async fn live_invocations(&self, service: &str, key: &str) -> Result<Vec<Invocation>> {
        self.query(&format!(
            "SELECT id, status, pinned_deployment_id, invoked_by_id, last_failure, retry_count \
             FROM sys_invocation WHERE target_service_name = {} AND target_service_key = {} \
             AND status <> 'completed' ORDER BY created_at",
            sql_literal(&self.service_name(service)),
            sql_literal(key)
        ))
        .await
    }

    /// Every deployment the server holds.
    pub async fn deployments(&self) -> Result<Vec<Deployment>> {
        #[derive(Deserialize)]
        struct Listing {
            deployments: Vec<Listed>,
        }
        #[derive(Deserialize)]
        struct Listed {
            id: String,
            #[serde(default)]
            uri: Option<String>,
        }
        let url = format!("{}/deployments", self.admin_url);
        let listing: Listing = self
            .http
            .get(&url)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
            .with_context(|| format!("GET {url}"))?
            .json()
            .await
            .with_context(|| format!("decode GET {url}"))?;
        Ok(listing
            .deployments
            .into_iter()
            .filter_map(|listed| {
                listed.uri.map(|endpoint| Deployment {
                    id: listed.id,
                    endpoint,
                })
            })
            .collect())
    }

    /// The deployment registered at `uri`.
    pub async fn deployment_at(&self, uri: &str) -> Result<Deployment> {
        let wanted = uri.trim_end_matches('/');
        self.deployments()
            .await?
            .into_iter()
            .find(|deployment| deployment.endpoint.trim_end_matches('/') == wanted)
            .ok_or_else(|| anyhow!("no deployment is registered at {uri}"))
    }

    /// Resume a paused invocation on the deployment it is pinned to, as an
    /// operator does once the build its journal belongs to serves again.
    pub async fn resume(&self, invocation_id: &str) -> Result<()> {
        let url = format!("{}/invocations/{invocation_id}/resume", self.admin_url);
        let response = self
            .http
            .patch(&url)
            .send()
            .await
            .with_context(|| format!("PATCH {url}"))?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            bail!("resuming {invocation_id} answered {status}: {body}");
        }
        Ok(())
    }

    /// Every invocation of `service`'s `handler` whose object or workflow
    /// key contains `fragment`, oldest first.
    pub async fn invocations_like(
        &self,
        service: &str,
        fragment: &str,
        handler: &str,
    ) -> Result<Vec<(String, Invocation)>> {
        #[derive(Deserialize)]
        struct Row {
            target_service_key: String,
            #[serde(flatten)]
            invocation: Invocation,
        }
        let rows: Vec<Row> = self
            .query(&format!(
                "SELECT target_service_key, id, status, pinned_deployment_id, invoked_by_id, \
                 last_failure, retry_count FROM sys_invocation WHERE target_service_name = {} \
                 AND target_service_key LIKE {} AND target_handler_name = {} ORDER BY created_at",
                sql_literal(&self.service_name(service)),
                sql_literal(&format!("%{fragment}%")),
                sql_literal(handler)
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|row| (row.target_service_key, row.invocation))
            .collect())
    }

    /// Every segment `run` of `process_id`, on any lane of lash's process
    /// workflow, oldest first: the lane it was sent on, its workflow key
    /// (`<pid>` for segment 0, `<pid>#<n>` after a hand-over) and its
    /// invocation.
    pub async fn process_segments(&self, process_id: &str) -> Result<Vec<ProcessSegment>> {
        self.query(&format!(
            "SELECT target_service_name AS lane, target_service_key AS key, id, status, \
             pinned_deployment_id, invoked_by_id, last_failure, retry_count FROM sys_invocation \
             WHERE target_service_name LIKE {} AND (target_service_key = {} OR \
             target_service_key LIKE {}) AND target_handler_name = 'run' ORDER BY created_at",
            sql_literal(&format!("{}%", self.service_name("LashProcessWorkflow"))),
            sql_literal(process_id),
            sql_literal(&format!("{process_id}#%"))
        ))
        .await
    }
}
