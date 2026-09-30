//! Run-owned collection. All elapsed spans use the driver's monotonic clock.

use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use sqlx::PgPool;
use std::io::Write as _;
use std::time::Instant;

pub struct Collector {
    pub run: String,
    pub workers: Vec<String>,
    pub nodes: Vec<String>,
    pub admin: String,
    pub client: reqwest::Client,
    pub database: PgPool,
    pub started: Instant,
    pub initial_invocations: tokio::sync::Mutex<Option<std::collections::BTreeSet<String>>>,
}

impl Collector {
    pub async fn query(&self, query: &str) -> Result<Vec<Value>> {
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = async {
                let response = self
                    .client
                    .post(format!("{}/query", self.admin))
                    .timeout(deadline.saturating_duration_since(Instant::now()))
                    .header("accept", "application/json")
                    .json(&json!({"query": query}))
                    .send()
                    .await?;
                let status = response.status();
                let body = response.text().await?;
                Ok::<_, reqwest::Error>((status, body))
            }
            .await;
            let (status, body) = match response {
                Ok(response) => response,
                Err(error) if Instant::now() < deadline && query_transport_retryable(&error) => {
                    self.query_retry(query, attempt, format!("{error:#}"))?;
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if status.as_u16() == 500
                && (body.contains("partition store")
                    || body.contains("partition is being transferred"))
                && Instant::now() < deadline
            {
                self.query_retry(query, attempt, body)?;
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                continue;
            }
            ensure!(
                status.is_success(),
                "Restate query `{query}` returned {status}: {body}"
            );
            let response: Value = serde_json::from_str(&body)?;
            return response["rows"]
                .as_array()
                .cloned()
                .context("Restate query has no rows");
        }
    }

    fn query_retry(&self, query: &str, attempt: u32, error: String) -> Result<()> {
        emit(
            &json!({"schema_version": 1, "record": "query_retry", "run": self.run,
            "monotonic_ns": self.started.elapsed().as_nanos(), "attempt": attempt,
            "query_kind": if query.contains("sys_journal") { "journal" } else { "invocation_census" },
            "error": error}),
        )
    }

    /// Retain completed journals long enough to read them in the isolated topology.
    pub async fn retain_journals(&self, retention: &str) -> Result<Value> {
        let services: Value = self
            .client
            .get(format!("{}/services", self.admin))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let services = services["services"]
            .as_array()
            .context("service metadata is missing")?;
        ensure!(!services.is_empty(), "no services are registered");
        let mut configured = Vec::new();
        for service in services {
            let name = service["name"].as_str().context("service has no name")?;
            let settings: Value = self
                .client
                .patch(format!("{}/services/{name}", self.admin))
                .json(&json!({"journal_retention": retention}))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            configured.push(json!({"before": service, "after": settings}));
        }
        Ok(
            json!({"journal_retention": retention, "services": configured,
            "default_settings_divergence": "completed journals retained for measurement; baseline decision required"}),
        )
    }

    pub async fn journal(&self, id: &str) -> Result<Value> {
        let id = id.replace('\'', "''");
        let rows = self.query(&format!(
            "SELECT COUNT(*) AS entries, COALESCE(SUM(raw_length), 0) AS bytes FROM sys_journal WHERE id = '{id}'"
        )).await?;
        rows.into_iter()
            .next()
            .context("journal aggregate is missing")
    }

    async fn invocations(&self) -> Result<Vec<Value>> {
        let mut result = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let predicate = cursor.as_ref().map_or_else(String::new, |id| {
                format!(" WHERE id > '{}'", id.replace('\'', "''"))
            });
            let rows = self.query(&format!(
                "SELECT id, target_service_name, target_service_key, target_handler_name, invoked_by_id, status, pinned_deployment_id, idempotency_key, journal_retention, completion_retention FROM sys_invocation_status{predicate} ORDER BY id LIMIT 128"
            )).await?;
            for row in &rows {
                let id = row["id"]
                    .as_str()
                    .context("census row has no invocation ID")?;
                ensure!(
                    cursor
                        .as_ref()
                        .is_none_or(|previous| id > previous.as_str()),
                    "census page did not advance"
                );
                cursor = Some(id.to_owned());
            }
            let finished = rows.len() < 128;
            result.extend(rows);
            if finished {
                return Ok(result);
            }
        }
    }

    async fn owned_journals(
        &self,
        owned: &std::collections::BTreeSet<String>,
    ) -> Result<(Vec<Value>, Vec<Value>)> {
        let owned = owned.iter().collect::<Vec<_>>();
        let mut aggregates = Vec::new();
        let mut commands = Vec::new();
        for chunk in owned.chunks(64) {
            let ids = chunk
                .iter()
                .map(|id| format!("'{}'", id.replace('\'', "''")))
                .collect::<Vec<_>>()
                .join(",");
            aggregates.extend(self.query(&format!(
                "SELECT id, COUNT(*) AS entries, SUM(raw_length) AS bytes FROM sys_journal WHERE id IN ({ids}) GROUP BY id"
            )).await?);
            commands.extend(self.query(&format!(
                "SELECT id, index, entry_type, version, entry_json FROM sys_journal WHERE id IN ({ids}) AND (entry_type = 'Command: SendSignal' OR entry_type LIKE '%CancelInvocation%')"
            )).await?);
        }
        Ok((aggregates, commands))
    }

    pub async fn sample(&self) -> Result<Value> {
        let start = self.started.elapsed().as_nanos();
        let mut workers = Vec::new();
        for worker in &self.workers {
            let value: Value = self
                .client
                .get(format!("{worker}/load/resources"))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            workers.push(json!({"node": worker, "resources": value}));
        }
        // Read the durable table directly. The sys_invocation view joins
        // ephemeral leader state, which can disappear during rebalancing.
        // Retry counts come from cumulative counters with explicit epochs.
        let invocations = self.invocations().await?;
        // Each topology has one driver and no other traffic after the L2
        // smoke drains. New invocations include HTTP-started child processes
        // whose invoked_by_id is absent. Capture the census before admission.
        let mut initial = self.initial_invocations.lock().await;
        let initial = initial.get_or_insert_with(|| {
            invocations
                .iter()
                .filter_map(|row| row["id"].as_str().map(str::to_owned))
                .collect()
        });
        let mut owned = std::collections::BTreeSet::new();
        loop {
            let before = owned.len();
            for invocation in &invocations {
                let key = invocation["target_service_key"]
                    .as_str()
                    .unwrap_or_default();
                let parent = invocation["invoked_by_id"].as_str().unwrap_or_default();
                if !initial.contains(invocation["id"].as_str().unwrap_or_default())
                    || key.contains(&format!("load-{}-", self.run))
                    || owned.contains(parent)
                {
                    owned.insert(
                        invocation["id"]
                            .as_str()
                            .context("invocation has no id")?
                            .to_owned(),
                    );
                }
            }
            if owned.len() == before {
                break;
            }
        }
        let mut journals = Vec::new();
        let mut cancellation_commands = Vec::new();
        if !owned.is_empty() {
            let (aggregates, commands) = self.owned_journals(&owned).await?;
            cancellation_commands = commands;
            for id in &owned {
                let row = aggregates.iter().find(|row| row["id"] == *id);
                journals.push(json!({"id": id, "journal": {
                    "entries": row.map_or(json!(0), |row| row["entries"].clone()),
                    "bytes": row.map_or(json!(0), |row| row["bytes"].clone()),
                }}));
            }
        }
        let mut nodes = Vec::new();
        for node in &self.nodes {
            let metrics = self
                .client
                .get(format!("{node}/metrics"))
                .send()
                .await?
                .error_for_status()?
                .text()
                .await?;
            let physical: Value = self
                .client
                .get(format!("{}/physical", node.replace(":5122", ":18102")))
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            nodes.push(json!({"node": node, "prometheus": metrics, "physical": physical}));
        }
        // The admin SQL API exposes invocation data only. Cluster tables are
        // queried through the pinned NodeCtl client in the physical collector.
        let cluster = nodes.first().context("no Restate collector nodes")?;
        let node_epochs = &cluster["physical"]["node_epochs"];
        let leader_epochs = &cluster["physical"]["leader_epochs"];
        let postgres: Value = sqlx::query_scalar(
            "SELECT json_build_object(
                'epoch', pg_postmaster_start_time()::text,
                'stats_reset', d.stats_reset::text,
                'transactions', d.xact_commit + d.xact_rollback,
                'blocks_read', d.blks_read, 'blocks_hit', d.blks_hit,
                'read_ms', d.blk_read_time, 'write_ms', d.blk_write_time,
                'connections', d.numbackends,
                'wal_bytes', (SELECT wal_bytes FROM pg_stat_wal),
                'wal_reset', (SELECT stats_reset::text FROM pg_stat_wal),
                'lock_waiters', (SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock'),
                'waiters', (SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event IS NOT NULL),
                'query_calls', (SELECT coalesce(sum(calls),0) FROM pg_stat_statements WHERE dbid=d.datid),
                'query_ms', (SELECT coalesce(sum(total_exec_time),0) FROM pg_stat_statements WHERE dbid=d.datid),
                'query_reset', (SELECT stats_reset::text FROM pg_stat_statements_info)
             ) FROM pg_stat_database d WHERE datname=current_database()"
        ).fetch_one(&self.database).await.context("sample PostgreSQL counters")?;
        Ok(
            json!({"schema_version": 1, "record": "sample", "run": self.run,
            "monotonic_ns": start, "collection_finished_ns": self.started.elapsed().as_nanos(),
            "workers": workers, "postgres": postgres, "invocations": invocations,
            "journals": journals, "cancellation_commands": cancellation_commands,
            "census": {"invocation_page_size": 128, "journal_chunk_size": 64,
                "consistency": "ordered scan over collector interval; final drain resamples until settled"},
            "restate": nodes, "node_epochs": node_epochs, "leader_epochs": leader_epochs}),
        )
    }
}

fn query_transport_retryable(error: &reqwest::Error) -> bool {
    error.is_connect()
        || error.is_timeout()
        || error.is_request()
        || error.is_body()
        || error.is_decode()
}

/// One JSON record per line in the run's file. Container logs are progress only.
pub fn emit(value: &Value) -> Result<()> {
    ensure!(
        value["schema_version"] == 1,
        "measurement schema version is missing"
    );
    let path = crate::required_env("LASH_LOAD_MEASUREMENTS_PATH")?;
    let row = format!("load measurement {}\n", serde_json::to_string(value)?);
    static WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _writer = WRITER
        .lock()
        .map_err(|_| anyhow::anyhow!("measurement writer is poisoned"))?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)?
        .write_all(row.as_bytes())?;
    println!(
        "load collection record={} bytes={}",
        value["record"],
        row.len()
    );
    Ok(())
}

/// Preserve the independent ledgers before the topology and database are removed.
pub async fn retain_witness(pool: &PgPool, run: &str) -> Result<()> {
    for (table, predicate) in [
        ("witness_load_events", "run_id = $1"),
        ("witness_load_faults", "run_id = $1"),
        (
            "witness_provider_receipts",
            "left(workflow_id, length($1) + 1) = $1 || '/'",
        ),
        (
            "witness_effect_attempts",
            "left(logical_key, length($1) + 1) = $1 || '/'",
        ),
        (
            "witness_effect_commits",
            "left(logical_key, length($1) + 1) = $1 || '/'",
        ),
        (
            "witness_effect_replies",
            "left(logical_key, length($1) + 1) = $1 || '/'",
        ),
    ] {
        let rows: Vec<Value> = sqlx::query_scalar(&format!(
            "SELECT to_jsonb(evidence) FROM {table} evidence WHERE {predicate} ORDER BY recorded_at_us"
        )).bind(run).fetch_all(pool).await?;
        emit(
            &json!({"schema_version": 1, "record": "witness_evidence", "run": run,
            "ledger": table, "rows": rows, "clock": "witness PostgreSQL recorded_at_us"}),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    async fn fixture_collector(
        router: axum::Router,
    ) -> Result<(Collector, tokio::task::JoinHandle<()>)> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("fixture server");
        });
        Ok((
            Collector {
                run: "fixture".to_owned(),
                workers: Vec::new(),
                nodes: Vec::new(),
                admin: format!("http://{address}"),
                client: reqwest::Client::new(),
                database: sqlx::postgres::PgPoolOptions::new()
                    .connect_lazy("postgres://fixture@localhost/fixture")?,
                started: Instant::now(),
                initial_invocations: tokio::sync::Mutex::new(None),
            },
            task,
        ))
    }

    #[tokio::test]
    async fn invocation_census_pages_every_id() -> Result<()> {
        async fn query(
            axum::Json(body): axum::Json<Value>,
        ) -> (axum::http::StatusCode, axum::Json<Value>) {
            let sql = body["query"].as_str().expect("query string");
            if !sql.ends_with("ORDER BY id LIMIT 128") {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "unbounded census"})),
                );
            }
            let after = sql
                .split("WHERE id > '")
                .nth(1)
                .map(|s| s.split('\'').next().expect("cursor"));
            let rows = (0..270)
                .map(|i| format!("inv{i:04}"))
                .filter(|id| after.is_none_or(|cursor| id.as_str() > cursor))
                .take(128)
                .map(|id| json!({"id": id, "status": "completed"}))
                .collect::<Vec<_>>();
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({"rows": rows})),
            )
        }
        let (collector, server) =
            fixture_collector(axum::Router::new().route("/query", axum::routing::post(query)))
                .await?;
        let result = collector.invocations().await;
        server.abort();
        let rows = result?;
        assert_eq!(
            rows.iter()
                .map(|r| r["id"].as_str().expect("id"))
                .collect::<Vec<_>>(),
            (0..270).map(|i| format!("inv{i:04}")).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn journal_chunks_preserve_aggregates_and_cancellation_commands() -> Result<()> {
        async fn query(
            axum::Json(body): axum::Json<Value>,
        ) -> (axum::http::StatusCode, axum::Json<Value>) {
            let sql = body["query"].as_str().expect("query string");
            let ids = sql
                .split("id IN (")
                .nth(1)
                .expect("ID filter")
                .split(')')
                .next()
                .expect("end filter")
                .split(',')
                .map(|id| id.trim_matches('\''))
                .collect::<Vec<_>>();
            if ids.len() > 64 {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({"error": "unbounded journal read"})),
                );
            }
            let rows = ids
                .into_iter()
                .map(|id| {
                    if sql.contains("GROUP BY id") {
                        json!({"id": id, "entries": 3, "bytes": 40})
                    } else {
                        json!({"id": id, "index": 2, "entry_type": "Command: SendSignal"})
                    }
                })
                .collect::<Vec<_>>();
            (
                axum::http::StatusCode::OK,
                axum::Json(json!({"rows": rows})),
            )
        }
        let (collector, server) =
            fixture_collector(axum::Router::new().route("/query", axum::routing::post(query)))
                .await?;
        let owned = (0..150).map(|i| format!("inv{i:04}")).collect();
        let result = collector.owned_journals(&owned).await;
        server.abort();
        let (aggregates, commands) = result?;
        assert_eq!(aggregates.len(), 150);
        assert_eq!(commands.len(), 150);
        assert_eq!(
            aggregates
                .iter()
                .map(|r| r["id"].as_str().expect("id").to_owned())
                .collect::<std::collections::BTreeSet<_>>(),
            owned
        );
        assert_eq!(
            commands
                .iter()
                .map(|r| r["id"].as_str().expect("id").to_owned())
                .collect::<std::collections::BTreeSet<_>>(),
            owned
        );
        assert!(
            aggregates
                .iter()
                .all(|r| r["entries"] == 3 && r["bytes"] == 40)
        );
        Ok(())
    }

    #[tokio::test]
    async fn interrupted_chunked_query_body_is_retryable() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            let mut request = String::new();
            let mut reader = tokio::io::BufReader::new(&mut socket);
            while !request.ends_with("\r\n\r\n") {
                ensure!(
                    reader.read_line(&mut request).await? > 0,
                    "query request closed before its headers"
                );
            }
            drop(reader);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\n{\"r")
                .await?;
            socket.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        });
        let error = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()?
            .get(format!("http://{address}/query"))
            .send()
            .await?
            .text()
            .await
            .expect_err("the query body was interrupted before its complete chunk");
        server.await??;
        assert!(query_transport_retryable(&error), "{error:#}");
        Ok(())
    }
}
