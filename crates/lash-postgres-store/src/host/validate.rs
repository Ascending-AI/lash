//! [`PostgresHostConfig::validate`]: every rule a host configuration keeps,
//! checked before any connection opens, and the connection count it costs.

use std::time::Duration;

use lash_durable::{DurableConfig, Notifier};

use super::config::{
    DedicatedConnectionPolicy, DeploymentBudget, LiveReplayPolicy, PoolPolicy, PostgresHostConfig,
    ReconnectPolicy, RetryPolicy, ServerTimeout, TransactionGuards,
};
use crate::PostgresConnectionBudget;

/// A host configuration value that breaks a rule, named by its serialized
/// path (`roles.work.max_connections`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostgresHostConfigError {
    /// The refused field's serialized path.
    pub field: String,
    /// What the field must be.
    pub reason: String,
}

impl std::fmt::Display for PostgresHostConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "postgres host config `{}` {}", self.field, self.reason)
    }
}

impl std::error::Error for PostgresHostConfigError {}

type Checked = Result<(), PostgresHostConfigError>;

fn refuse(field: impl Into<String>, reason: impl Into<String>) -> Checked {
    Err(PostgresHostConfigError {
        field: field.into(),
        reason: reason.into(),
    })
}

/// The longest limit PostgreSQL's millisecond timeout settings carry.
const SERVER_TIMEOUT_MAX: Duration = Duration::from_millis(i32::MAX as u64);

const APPLICATION_NAME_PREFIX_MAX: usize = 32;
const REPLAY_SCHEMA_MAX: usize = 48;

/// A duration of at least one whole millisecond that fits `u64` ms.
fn millis(field: &str, value: Duration) -> Checked {
    if value < Duration::from_millis(1) {
        return refuse(field, "must be at least 1 ms");
    }
    if !value.subsec_nanos().is_multiple_of(1_000_000) {
        return refuse(field, "must be a whole number of milliseconds");
    }
    if u64::try_from(value.as_millis()).is_err() {
        return refuse(field, "does not fit u64 milliseconds");
    }
    Ok(())
}

fn pool(field: &str, policy: &PoolPolicy) -> Checked {
    if policy.max_connections == 0 {
        return refuse(format!("{field}.max_connections"), "must be at least 1");
    }
    if policy.min_connections > policy.max_connections {
        return refuse(
            format!("{field}.min_connections"),
            "must be at most max_connections",
        );
    }
    millis(
        &format!("{field}.acquire_timeout_ms"),
        policy.acquire_timeout,
    )?;
    if let Some(idle) = policy.idle_timeout {
        millis(&format!("{field}.idle_timeout_ms"), idle)?;
    }
    if let Some(lifetime) = policy.max_lifetime {
        millis(&format!("{field}.max_lifetime_ms"), lifetime)?;
    }
    Ok(())
}

fn dedicated(field: &str, policy: &DedicatedConnectionPolicy) -> Checked {
    millis(
        &format!("{field}.acquire_timeout_ms"),
        policy.acquire_timeout,
    )
}

fn server_timeout(field: &str, timeout: ServerTimeout) -> Checked {
    if let ServerTimeout::Limit(limit) = timeout {
        millis(field, limit)?;
        if limit > SERVER_TIMEOUT_MAX {
            return refuse(field, format!("must be at most {} ms", i32::MAX));
        }
    }
    Ok(())
}

fn guards(field: &str, guards: &TransactionGuards) -> Checked {
    server_timeout(&format!("{field}.lock_timeout"), guards.lock)?;
    server_timeout(&format!("{field}.statement_timeout"), guards.statement)?;
    server_timeout(
        &format!("{field}.idle_in_transaction_timeout"),
        guards.idle_in_transaction,
    )?;
    server_timeout(&format!("{field}.transaction_timeout"), guards.transaction)?;
    if let Some(deadline) = guards.operation_deadline {
        millis(&format!("{field}.operation_deadline_ms"), deadline)?;
    }
    // A lock wait is part of a statement, and a statement part of the
    // operation: an inner limit at or past its outer one never fires.
    if let (Some(lock), Some(statement)) = (guards.lock.limit(), guards.statement.limit())
        && lock >= statement
    {
        return refuse(
            format!("{field}.lock_timeout"),
            "must be shorter than statement_timeout",
        );
    }
    if let (Some(statement), Some(deadline)) = (guards.statement.limit(), guards.operation_deadline)
        && statement > deadline
    {
        return refuse(
            format!("{field}.statement_timeout"),
            "must be at most operation_deadline_ms",
        );
    }
    Ok(())
}

fn retry(field: &str, policy: &RetryPolicy) -> Checked {
    if policy.attempts == 0 {
        return refuse(format!("{field}.attempts"), "must be at least 1");
    }
    millis(&format!("{field}.initial_delay_ms"), policy.initial_delay)?;
    millis(&format!("{field}.max_delay_ms"), policy.max_delay)?;
    if policy.max_delay < policy.initial_delay {
        return refuse(
            format!("{field}.max_delay_ms"),
            "must be at least initial_delay_ms",
        );
    }
    Ok(())
}

fn reconnect(field: &str, policy: &ReconnectPolicy) -> Checked {
    millis(&format!("{field}.initial_delay_ms"), policy.initial_delay)?;
    millis(&format!("{field}.max_delay_ms"), policy.max_delay)?;
    if policy.max_delay < policy.initial_delay {
        return refuse(
            format!("{field}.max_delay_ms"),
            "must be at least initial_delay_ms",
        );
    }
    Ok(())
}

fn within(field: &str, value: Duration, low: Duration, high: Duration) -> Checked {
    if value < low || value > high {
        return refuse(
            field,
            format!(
                "must be between {} and {} ms",
                low.as_millis(),
                high.as_millis()
            ),
        );
    }
    Ok(())
}

fn count(field: &str, value: usize, low: usize, high: usize) -> Checked {
    if !(low..=high).contains(&value) {
        return refuse(field, format!("must be between {low} and {high}"));
    }
    Ok(())
}

fn live_replay(policy: &LiveReplayPolicy) -> Checked {
    let data = &policy.data;
    let schema_ok = data.schema.len() <= REPLAY_SCHEMA_MAX
        && data
            .schema
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first == '_')
        && data
            .schema
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !schema_ok {
        return refuse(
            "live_replay.data.schema",
            format!(
                "must be a lowercase identifier ([a-z_][a-z0-9_]*) of at most {REPLAY_SCHEMA_MAX} characters"
            ),
        );
    }
    within(
        "live_replay.data.publish_tick_ms",
        data.publish_tick,
        Duration::ZERO,
        Duration::from_secs(1),
    )?;
    within(
        "live_replay.data.max_age_ms",
        data.max_age,
        Duration::from_millis(1),
        Duration::from_secs(24 * 3600),
    )?;
    within(
        "live_replay.data.cleanup_interval_ms",
        data.cleanup_interval,
        Duration::from_millis(100),
        Duration::from_secs(3600),
    )?;
    within(
        "live_replay.data.cleanup_jitter_ms",
        data.cleanup_jitter,
        Duration::ZERO,
        data.cleanup_interval,
    )?;
    count(
        "live_replay.data.max_batch_events",
        data.max_batch_events,
        1,
        65_536,
    )?;
    count(
        "live_replay.data.max_events_per_session",
        data.max_events_per_session,
        1,
        1_000_000,
    )?;
    count(
        "live_replay.data.max_bytes_per_session",
        data.max_bytes_per_session,
        1024,
        1024 * 1024 * 1024,
    )?;
    count(
        "live_replay.data.cleanup_batch",
        data.cleanup_batch,
        1,
        65_536,
    )?;
    pool("live_replay.pool", &policy.pool)?;
    if data.publish_concurrency == 0
        || data.publish_concurrency > policy.pool.max_connections as usize
    {
        return refuse(
            "live_replay.data.publish_concurrency",
            "must be between 1 and live_replay.pool.max_connections",
        );
    }
    dedicated("live_replay.listener", &policy.listener)?;
    reconnect("live_replay.reconnect", &policy.reconnect)
}

fn deployment(budget: &DeploymentBudget, per_process: u32) -> Checked {
    if budget.processes_per_generation == 0 {
        return refuse("deployment.processes_per_generation", "must be at least 1");
    }
    if budget.generations < 2 {
        return refuse(
            "deployment.generations",
            "must be at least 2: a rollout overlaps the old and new release",
        );
    }
    if budget
        .connection_budget(per_process)
        .peak_connections()
        .is_err()
    {
        return refuse("deployment", "the declared peak overflows");
    }
    Ok(())
}

impl DeploymentBudget {
    /// The server-wide budget of a process that opens `per_process`
    /// connections.
    pub fn connection_budget(&self, per_process: u32) -> PostgresConnectionBudget {
        PostgresConnectionBudget {
            processes_per_generation: self.processes_per_generation,
            pool_max: per_process,
            generations: self.generations,
            workers: self.other_clients,
            admin_headroom: self.admin_headroom,
        }
    }
}

impl PostgresHostConfig {
    /// Refuse the first value that breaks a rule, before any I/O, and
    /// answer the validated durable settings.
    ///
    /// # Errors
    ///
    /// [`PostgresHostConfigError`] naming the field.
    pub fn validate(&self) -> Result<DurableConfig, PostgresHostConfigError> {
        let prefix = &self.connection.application_name_prefix;
        if prefix.is_empty()
            || prefix.len() > APPLICATION_NAME_PREFIX_MAX
            || !prefix
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            refuse(
                "connection.application_name_prefix",
                format!("must be 1 to {APPLICATION_NAME_PREFIX_MAX} characters of [A-Za-z0-9_-]"),
            )?;
        }
        let transport = &self.connection.transport;
        if transport.ssl_client_cert.is_some() != transport.ssl_client_key.is_some() {
            refuse(
                "connection.transport.ssl_client_cert",
                "must be given together with ssl_client_key",
            )?;
        }

        let roles = &self.roles;
        pool("roles.work", &roles.work)?;
        pool("roles.scheduler", &roles.scheduler)?;
        pool("roles.critical", &roles.critical)?;
        dedicated("roles.renewal", &roles.renewal)?;
        dedicated("roles.listener", &roles.listener)?;
        if roles.served_nodes == 0 {
            refuse("roles.served_nodes", "must be at least 1")?;
        }
        if roles.max_store_operations == 0
            || roles.max_store_operations > roles.work.max_connections as usize
        {
            refuse(
                "roles.max_store_operations",
                "must be between 1 and roles.work.max_connections",
            )?;
        }

        let durable = self
            .node
            .validate()
            .map_err(|error| PostgresHostConfigError {
                field: "node".to_owned(),
                reason: error.to_string(),
            })?;

        let guard = &self.guards;
        guards("guards.ordinary", &guard.ordinary)?;
        guards("guards.durable", &guard.durable)?;
        guards("guards.renewal", &guard.renewal)?;
        guards("guards.scheduler", &guard.scheduler)?;
        guards("guards.replay", &guard.replay)?;
        guards("guards.inspection", &guard.inspection)?;
        millis("guards.store_startup_ms", guard.store_startup)?;
        for (field, deadline) in [
            ("guards.ordinary", guard.ordinary.operation_deadline),
            ("guards.durable", guard.durable.operation_deadline),
            ("guards.renewal", guard.renewal.operation_deadline),
            ("guards.scheduler", guard.scheduler.operation_deadline),
            ("guards.replay", guard.replay.operation_deadline),
        ] {
            if deadline.is_none() {
                refuse(
                    format!("{field}.operation_deadline_ms"),
                    "is required for runtime work",
                )?;
            }
        }
        // A renewal must finish, acquire included, before the next one is
        // due, and a late one must still leave a renewal before self-stop.
        let lease = self.node.lease;
        let renewal_deadline = guard.renewal.operation_deadline.unwrap_or(Duration::MAX);
        if roles.renewal.acquire_timeout >= renewal_deadline {
            refuse(
                "roles.renewal.acquire_timeout_ms",
                "must be shorter than guards.renewal.operation_deadline_ms",
            )?;
        }
        if renewal_deadline >= lease.heartbeat_every {
            refuse(
                "guards.renewal.operation_deadline_ms",
                "must be shorter than node.lease.heartbeat_every_ms",
            )?;
        }
        if lease.heartbeat_every.saturating_add(renewal_deadline) >= lease.self_stop_after {
            refuse(
                "guards.renewal.operation_deadline_ms",
                "plus node.lease.heartbeat_every_ms must be shorter than node.lease.self_stop_after_ms",
            )?;
        }

        retry("retry.store", &self.retry.store)?;
        retry("retry.durable", &self.retry.durable)?;
        retry("retry.wait_resolution", &self.retry.wait_resolution)?;
        retry("retry.live_replay", &self.retry.live_replay)?;
        reconnect("signals.reconnect", &self.signals.reconnect)?;

        if let Some(replay) = &self.live_replay {
            live_replay(replay)?;
        }

        let maintenance = &self.maintenance;
        pool("maintenance.preflight_pool", &maintenance.preflight_pool)?;
        pool("maintenance.migration_pool", &maintenance.migration_pool)?;
        millis(
            "maintenance.migration_lock_timeout_ms",
            maintenance.migration_lock_timeout,
        )?;
        server_timeout(
            "maintenance.migration_statement_timeout",
            maintenance.migration_statement_timeout,
        )?;
        if let Some(deadline) = maintenance.migration_deadline {
            millis("maintenance.migration_deadline_ms", deadline)?;
        }
        millis(
            "maintenance.sweep_liveness_probe_timeout_ms",
            maintenance.sweep_liveness_probe_timeout,
        )?;
        for (field, value) in [
            (
                "maintenance.migration_batch_rows",
                maintenance.migration_batch_rows,
            ),
            (
                "maintenance.max_sweep_sessions",
                maintenance.max_sweep_sessions,
            ),
            (
                "maintenance.max_schema_sessions",
                maintenance.max_schema_sessions,
            ),
            (
                "maintenance.checkpoint_ref_chunk",
                maintenance.checkpoint_ref_chunk,
            ),
            (
                "maintenance.sweep_mint_attempts",
                maintenance.sweep_mint_attempts,
            ),
            (
                "maintenance.process_event_release_page_rows",
                maintenance.process_event_release_page_rows,
            ),
        ] {
            if value == 0 {
                refuse(field, "must be at least 1")?;
            }
        }

        let per_process =
            self.connections_per_process()
                .ok_or_else(|| PostgresHostConfigError {
                    field: "roles".to_owned(),
                    reason: "the per-process connection count overflows".to_owned(),
                })?;
        if let Some(budget) = &self.deployment {
            deployment(budget, per_process)?;
        }
        Ok(durable)
    }

    /// The server connections one process opens at most under this
    /// configuration: every pool at its maximum, each served node's renewal
    /// connection and listener session, the replay store's pool and
    /// listener, the detached schema and sweep sessions, and the
    /// deployment's declared other pools. The open's capacity probe and
    /// schema gate run on the work pool, so they add no session of their
    /// own. `None` when the sum overflows.
    pub fn connections_per_process(&self) -> Option<u32> {
        let roles = &self.roles;
        let listener = u32::from(matches!(self.node.notifier, Notifier::AfterCommit));
        let per_node = 1 + listener;
        let replay = self
            .live_replay
            .as_ref()
            .map_or(Some(0), |replay| replay.pool.max_connections.checked_add(1))?;
        let declared = self.deployment.as_ref().map_or(Some(0), |budget| {
            budget
                .other_host_connections
                .checked_add(budget.operator_connections)
        })?;
        [
            roles.work.max_connections,
            roles.scheduler.max_connections,
            roles.critical.max_connections,
            roles.served_nodes.checked_mul(per_node)?,
            replay,
            self.maintenance.max_schema_sessions,
            self.maintenance.max_sweep_sessions,
            declared,
        ]
        .into_iter()
        .try_fold(0_u32, u32::checked_add)
    }
}
