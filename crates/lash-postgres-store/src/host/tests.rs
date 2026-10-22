//! The host configuration's rules that need no database: validation before
//! any I/O, exact pool options, and endpoints that never print a secret.

use std::time::Duration;

use super::*;

/// A change that breaks one rule.
type BreakRule = Box<dyn Fn(&mut PostgresHostConfig)>;

fn refused(config: &PostgresHostConfig) -> String {
    config
        .validate()
        .expect_err("the configuration breaks a rule")
        .field
}

/// Every rule the study's validation list names refuses its field by its
/// serialized path, before anything connects (FIG-5240).
#[test]
fn a_configuration_that_breaks_a_rule_is_refused_by_its_field() {
    assert!(PostgresHostConfig::default().validate().is_ok());
    let cases: Vec<(&str, BreakRule)> = vec![
        (
            "roles.work.max_connections",
            Box::new(|c| c.roles.work.max_connections = 0),
        ),
        (
            "roles.scheduler.min_connections",
            Box::new(|c| c.roles.scheduler.min_connections = 2),
        ),
        (
            "roles.critical.acquire_timeout_ms",
            Box::new(|c| c.roles.critical.acquire_timeout = Duration::from_micros(1500)),
        ),
        (
            "roles.work.idle_timeout_ms",
            Box::new(|c| c.roles.work.idle_timeout = Some(Duration::ZERO)),
        ),
        ("roles.served_nodes", Box::new(|c| c.roles.served_nodes = 0)),
        (
            "roles.max_store_operations",
            Box::new(|c| c.roles.max_store_operations = 17),
        ),
        (
            "roles.max_store_operations",
            Box::new(|c| c.roles.max_store_operations = 0),
        ),
        (
            "node",
            Box::new(|c| c.node.claim_batch = c.node.max_active + 1),
        ),
        (
            "guards.durable.lock_timeout",
            Box::new(|c| c.guards.durable.lock = ServerTimeout::Limit(Duration::from_secs(6))),
        ),
        (
            "guards.ordinary.statement_timeout",
            Box::new(|c| c.guards.ordinary.operation_deadline = Some(Duration::from_secs(20))),
        ),
        (
            "guards.scheduler.idle_in_transaction_timeout",
            Box::new(|c| {
                c.guards.scheduler.idle_in_transaction =
                    ServerTimeout::Limit(Duration::from_millis(u64::from(u32::MAX)))
            }),
        ),
        (
            "guards.durable.operation_deadline_ms",
            Box::new(|c| c.guards.durable.operation_deadline = None),
        ),
        (
            "roles.renewal.acquire_timeout_ms",
            Box::new(|c| c.roles.renewal.acquire_timeout = Duration::from_secs(2)),
        ),
        (
            "guards.renewal.operation_deadline_ms",
            Box::new(|c| {
                c.guards.renewal.operation_deadline = Some(Duration::from_secs(3));
            }),
        ),
        (
            "guards.renewal.operation_deadline_ms",
            Box::new(|c| {
                c.node.lease.heartbeat_every = Duration::from_secs(8);
                c.guards.renewal.operation_deadline = Some(Duration::from_millis(2500));
            }),
        ),
        (
            "retry.store.attempts",
            Box::new(|c| c.retry.store.attempts = 0),
        ),
        (
            "retry.live_replay.max_delay_ms",
            Box::new(|c| c.retry.live_replay.max_delay = Duration::from_millis(1)),
        ),
        (
            "signals.reconnect.max_delay_ms",
            Box::new(|c| c.signals.reconnect.max_delay = Duration::from_millis(10)),
        ),
        (
            "live_replay.data.publish_concurrency",
            Box::new(|c| {
                let mut replay = LiveReplayPolicy::default();
                replay.data.publish_concurrency = 8;
                c.live_replay = Some(replay);
            }),
        ),
        (
            "live_replay.data.schema",
            Box::new(|c| {
                let mut replay = LiveReplayPolicy::default();
                replay.data.schema = "Live-Replay".into();
                c.live_replay = Some(replay);
            }),
        ),
        (
            "process_replay.data.schema",
            Box::new(|c| {
                let mut replay = ProcessReplayPolicy::default();
                replay.data.schema = "Process-Replay".into();
                c.process_replay = Some(replay);
            }),
        ),
        (
            "process_replay.data.max_retained_bytes",
            Box::new(|c| {
                let mut replay = ProcessReplayPolicy::default();
                replay.data.max_retained_bytes = replay.data.max_bytes_per_process as u64 - 1;
                c.process_replay = Some(replay);
            }),
        ),
        (
            "process_replay.data.reservation_bytes",
            Box::new(|c| {
                let mut replay = ProcessReplayPolicy::default();
                replay.data.reservation_bytes = replay.data.max_bytes_per_process + 1;
                c.process_replay = Some(replay);
            }),
        ),
        (
            "maintenance.max_sweep_sessions",
            Box::new(|c| c.maintenance.max_sweep_sessions = 0),
        ),
        (
            "connection.application_name_prefix",
            Box::new(|c| c.connection.application_name_prefix = "lash app".into()),
        ),
        (
            "connection.transport.ssl_client_cert",
            Box::new(|c| c.connection.transport.ssl_client_cert = Some("client.crt".into())),
        ),
        (
            "deployment.generations",
            Box::new(|c| {
                c.deployment = Some(DeploymentBudget {
                    processes_per_generation: 2,
                    generations: 1,
                    other_clients: 0,
                    admin_headroom: 3,
                    other_host_connections: 0,
                    operator_connections: 0,
                })
            }),
        ),
    ];
    for (field, break_rule) in cases {
        let mut config = PostgresHostConfig::default();
        break_rule(&mut config);
        assert_eq!(refused(&config), field, "{config:?}");
    }
}

/// The serialized form is milliseconds with unknown fields refused, and a
/// role's pool policy is given whole, so a partial object never takes
/// another role's defaults.
#[test]
fn the_configuration_serializes_in_milliseconds_and_refuses_unknown_fields() {
    let config = PostgresHostConfig::default();
    let json = serde_json::to_value(&config).expect("the default serializes");
    assert_eq!(json["roles"]["work"]["acquire_timeout_ms"], 30_000);
    assert_eq!(json["roles"]["work"]["idle_timeout_ms"], 600_000);
    assert_eq!(json["guards"]["durable"]["lock_timeout"], 2_000);
    assert_eq!(json["guards"]["durable"]["transaction_timeout"], "inherit");
    assert_eq!(json["node"]["lease"]["heartbeat_every_ms"], 3_000);
    let back: PostgresHostConfig = serde_json::from_value(json).expect("it reads back");
    assert_eq!(back, config);

    let parsed: PostgresHostConfig = serde_json::from_str(
        r#"{"roles": {"work": {"max_connections": 4, "min_connections": 0,
            "acquire_timeout_ms": 1000, "idle_timeout_ms": null, "max_lifetime_ms": null,
            "test_before_acquire": false}, "max_store_operations": 4},
            "guards": {"ordinary": {"lock_timeout": "disabled", "statement_timeout": "inherit",
            "idle_in_transaction_timeout": 5000, "transaction_timeout": "inherit",
            "operation_deadline_ms": 60000}}}"#,
    )
    .expect("a partial configuration parses");
    assert_eq!(parsed.roles.work.idle_timeout, None);
    assert_eq!(parsed.roles.scheduler, RolePolicies::default().scheduler);
    assert_eq!(parsed.guards.ordinary.lock, ServerTimeout::Disabled);
    assert!(parsed.validate().is_ok());
    for unknown in [
        r#"{"pool_max": 4}"#,
        r#"{"roles": {"reserve": {}}}"#,
        r#"{"roles": {"work": {"max_connections": 4}}}"#,
        r#"{"guards": {"durable": {"lock_timeout": "forever"}}}"#,
    ] {
        assert!(
            serde_json::from_str::<PostgresHostConfig>(unknown).is_err(),
            "{unknown} is refused"
        );
    }
}

/// D-RET limits as host configuration: the node's retirement serializes in
/// milliseconds and counts at the standard values, reads back exactly, and
/// one limit above its hard maximum is refused by the node field with the
/// durable settings' typed refusal, before anything connects.
#[test]
fn retirement_limits_are_host_configuration_refused_above_their_maxima() {
    use lash_durable::{DurableConfigError, RetirementSettings};

    let config = PostgresHostConfig::default();
    let json = serde_json::to_value(&config).expect("the default serializes");
    assert_eq!(
        json["node"]["retirement"],
        serde_json::json!({
            "every_ms": 1_000,
            "batch_nodes": 256,
            "steps_per_sweep": 16,
            "backlog_limit": 10_000,
        })
    );
    let mut above = json.clone();
    above["node"]["retirement"]["backlog_limit"] =
        serde_json::json!(RetirementSettings::MAX_BACKLOG_LIMIT + 1);
    let above: PostgresHostConfig = serde_json::from_value(above).expect("it parses");
    let refused = above.validate().expect_err("above the maximum");
    assert_eq!(refused.field, "node");
    assert_eq!(
        refused.reason,
        DurableConfigError::AboveMaximum {
            field: "retirement.backlog_limit",
            maximum: RetirementSettings::MAX_BACKLOG_LIMIT,
        }
        .to_string()
    );
    let mut at_maxima = config;
    at_maxima.node.retirement = RetirementSettings {
        every: Duration::from_millis(1),
        batch_nodes: RetirementSettings::MAX_BATCH_NODES,
        steps_per_sweep: RetirementSettings::MAX_STEPS_PER_SWEEP,
        backlog_limit: RetirementSettings::MAX_BACKLOG_LIMIT,
    };
    assert_eq!(
        at_maxima
            .validate()
            .map(|durable| durable.settings().retirement),
        Ok(at_maxima.node.retirement)
    );
}

/// `None` idle and lifetime reach SQLx as `None`: the pool never closes a
/// connection for idleness or age. The old builder skipped SQLx's setters
/// for `None`, so its defaults (10 min idle, 30 min lifetime) still applied.
#[test]
fn none_really_disables_idle_and_lifetime_on_every_pooled_role() {
    let factory = PostgresConnectionFactory::new(
        PostgresEndpoints::from_url("postgres://lash@127.0.0.1:1/lash").expect("parses"),
        ConnectionPolicy::default(),
    );
    let policy = PoolPolicy {
        idle_timeout: None,
        max_lifetime: None,
        ..RolePolicies::default().work
    };
    for role in [
        ConnectionRole::Work,
        ConnectionRole::Scheduler,
        ConnectionRole::Critical,
        ConnectionRole::Session,
        ConnectionRole::Replay,
        ConnectionRole::Preflight,
        ConnectionRole::Migration,
    ] {
        let pool = factory.pool(role, &policy, None);
        assert_eq!(pool.options().get_idle_timeout(), None, "{role}");
        assert_eq!(pool.options().get_max_lifetime(), None, "{role}");
    }
    let dedicated = factory.dedicated(
        ConnectionRole::Renewal,
        &RolePolicies::default().renewal,
        2,
        None,
    );
    assert_eq!(dedicated.options().get_idle_timeout(), None);
    assert_eq!(dedicated.options().get_max_lifetime(), None);
    assert_eq!(dedicated.options().get_max_connections(), 2);
}

/// A role's acquire timeout bounds a connect whose server never answers the
/// handshake.
#[tokio::test]
async fn a_stalled_handshake_is_bounded_by_the_role_acquire_timeout() {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("bind stalled Postgres peer");
    let port = listener.local_addr().expect("read listener address").port();
    let stalled_peer = tokio::spawn(async move {
        let (_socket, _) = listener.accept().await.expect("accept Postgres connection");
        std::future::pending::<()>().await;
    });
    let factory = PostgresConnectionFactory::new(
        PostgresEndpoints::from_url(&format!("postgres://postgres@127.0.0.1:{port}/postgres"))
            .expect("parses"),
        ConnectionPolicy::default(),
    );
    let pool = factory.pool(
        ConnectionRole::Work,
        &PoolPolicy {
            acquire_timeout: Duration::from_millis(100),
            ..RolePolicies::default().work
        },
        None,
    );
    let result = tokio::time::timeout(Duration::from_millis(400), pool.acquire()).await;
    stalled_peer.abort();
    assert!(
        matches!(result, Ok(Err(sqlx::Error::PoolTimedOut))),
        "acquire timeout did not bound the stalled handshake: {result:?}"
    );
}

/// What a host prints of its endpoints, and what a preflight reports as its
/// location, never carries the user, the password or the options.
#[test]
fn endpoints_and_reports_never_carry_credentials() {
    for url in [
        "postgres://lash:hunter2@db.internal:5432/lash?sslmode=require",
        "postgresql://admin:p%40ss%3Fw0rd@10.0.0.4/lash",
    ] {
        let endpoints = PostgresEndpoints::from_url(url)
            .expect("parses")
            .with_session_url(url)
            .expect("parses");
        for shown in [
            format!("{endpoints:?}"),
            crate::preflight::redact_options(endpoints.primary()),
        ] {
            for secret in ["hunter2", "p@ss", "lash:", "admin"] {
                assert!(!shown.contains(secret), "{shown} shows {secret}");
            }
        }
    }
    let error = PostgresEndpoints::from_url("postgres://lash:hunter2@db.internal:notaport/lash")
        .expect_err("a bad port does not parse");
    assert!(!error.to_string().contains("hunter2"), "{error}");
}

/// The guard prelude installs each limit with `BEGIN` and nothing for an
/// inherited one; `disabled` installs zero.
#[test]
fn a_prelude_installs_exactly_its_profile_with_begin() {
    let guards = TransactionGuards {
        lock: ServerTimeout::Limit(Duration::from_millis(250)),
        statement: ServerTimeout::Disabled,
        idle_in_transaction: ServerTimeout::Inherit,
        transaction: ServerTimeout::Inherit,
        operation_deadline: Some(Duration::from_secs(2)),
    };
    assert_eq!(
        TransactionPrelude::new(&guards).statement(),
        "BEGIN; SET LOCAL lock_timeout = 250; SET LOCAL statement_timeout = 0"
    );
    assert_eq!(TransactionPrelude::inherit().statement(), "BEGIN");
}

/// The sizing rule's worked example in `docs/operations/postgres.md`: one
/// default node opens at most 24 server connections, 32 with live replay,
/// 40 with process replay too, and the declared budget multiplies that by every overlapping process.
#[test]
fn the_sizing_rule_counts_every_session_a_process_opens() {
    let mut config = PostgresHostConfig::default();
    assert_eq!(config.connections_per_process(), Some(24));
    config.live_replay = Some(LiveReplayPolicy::default());
    assert_eq!(config.connections_per_process(), Some(32));
    config.process_replay = Some(ProcessReplayPolicy::default());
    assert_eq!(config.connections_per_process(), Some(40));
    config.process_replay = None;
    config.node.notifier = lash_durable::Notifier::PollOnly;
    assert_eq!(config.connections_per_process(), Some(31), "no listener");
    let budget = DeploymentBudget {
        processes_per_generation: 4,
        generations: 2,
        other_clients: 10,
        admin_headroom: 10,
        other_host_connections: 0,
        operator_connections: 0,
    };
    config.node.notifier = lash_durable::Notifier::AfterCommit;
    config.deployment = Some(budget);
    let per_process = config.connections_per_process().expect("fits");
    assert_eq!(per_process, 32, "declared other pools are zero");
    let peak = budget
        .connection_budget(per_process)
        .peak_connections()
        .expect("a valid budget");
    assert_eq!(peak, 276, "four replicas over two generations");
}

/// POSTGRES-NAME-POLICY: a deployment may narrow the reserved identifier budgets.
#[test]
fn deployment_name_limits_are_honored_before_io() {
    let mut config = PostgresHostConfig::standard();
    config.name_limits = PostgresNameLimits::new(1, 1).unwrap();
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("1 characters")
    );
    config.connection.application_name_prefix = "a".into();
    config.live_replay = Some(LiveReplayPolicy::default());
    assert!(
        config
            .validate()
            .unwrap_err()
            .to_string()
            .contains("at most 1")
    );
    config.live_replay.as_mut().unwrap().data.schema = "a".into();
    assert!(config.validate().is_ok());
    assert_eq!(
        serde_json::from_value::<PostgresHostConfig>(serde_json::to_value(&config).unwrap())
            .unwrap(),
        config
    );
}

/// Law S1: a rendered node never silently acquires new retirement defaults.
#[test]
fn offline_validation_names_the_missing_retirement_path() {
    let mut json = serde_json::to_value(PostgresHostConfig::standard()).unwrap();
    json["node"].as_object_mut().unwrap().remove("retirement");
    let error = validate_host_config(&json.to_string()).unwrap_err();
    assert_eq!(error.path, "node.retirement");
    assert_eq!(error.kind, HostConfigErrorKind::Missing);
}

/// Closed config objects identify the rejected key at any nesting depth.
#[test]
fn offline_validation_names_unknown_field_paths() {
    for (parent, path) in [
        ("", "typo"),
        ("/node", "node.typo"),
        ("/node/retirement", "node.retirement.typo"),
        ("/node/graph_reads", "node.graph_reads.typo"),
        (
            "/node/graph_reads/kernel_data",
            "node.graph_reads.kernel_data.typo",
        ),
        ("/roles/work", "roles.work.typo"),
        ("/name_limits", "name_limits.typo"),
    ] {
        let mut json = serde_json::to_value(PostgresHostConfig::standard()).unwrap();
        json.pointer_mut(parent).unwrap()["typo"] = serde_json::json!(1);
        let error = validate_host_config(&json.to_string()).unwrap_err();
        assert_eq!(error.path, path);
        assert_eq!(error.kind, HostConfigErrorKind::Unknown);
    }
}

/// Offline admission uses the connection path's parser and value rules,
/// including refusing trailing input rather than accepting its first value.
#[test]
fn offline_validation_applies_parser_and_value_rules() {
    assert!(validate_host_config("{}").is_ok());
    let json = serde_json::to_string(&PostgresHostConfig::standard()).unwrap();
    assert!(validate_host_config(&json).is_ok());
    for (json, path) in [
        (r#"{"roles":{"served_nodes":"one"}}"#, "roles.served_nodes"),
        (r#"{"roles":{"served_nodes":0}}"#, "roles.served_nodes"),
        (
            r#"{"connection":{"topology":"typo"}}"#,
            "connection.topology",
        ),
        ("{} {}", "$"),
        ("{", "$"),
    ] {
        let error = validate_host_config(json).unwrap_err();
        assert_eq!(error.path, path);
        assert_eq!(error.kind, HostConfigErrorKind::Invalid);
    }
}

/// The published structural contract admits serde's milliseconds, nullable
/// durations and timeout words, and refuses missing or unknown node fields.
#[test]
fn the_host_schema_agrees_with_serde_json_shapes() {
    let schema =
        lash_vocabulary::json_schema::JsonSchema::admit(serde_json::from_str(JSON_SCHEMA).unwrap())
            .unwrap();
    let standard = serde_json::to_value(PostgresHostConfig::standard()).unwrap();
    let development = serde_json::to_value(PostgresHostConfig::development()).unwrap();
    let mut cases = vec![serde_json::json!({}), standard.clone(), development];
    for timeout in [
        serde_json::json!("inherit"),
        serde_json::json!("disabled"),
        serde_json::json!(1_000),
    ] {
        let mut json = standard.clone();
        json["guards"]["ordinary"]["lock_timeout"] = timeout;
        json["roles"]["work"]["idle_timeout_ms"] = serde_json::Value::Null;
        cases.push(json);
    }
    let mut missing = standard.clone();
    missing["node"]
        .as_object_mut()
        .unwrap()
        .remove("retirement");
    cases.push(missing);
    let mut unknown = standard.clone();
    unknown["node"]["retirement"]["typo"] = serde_json::json!(1);
    cases.push(unknown);
    let mut seconds_object = standard;
    seconds_object["node"]["retirement"]["every_ms"] = serde_json::json!({"secs": 1, "nanos": 0});
    cases.push(seconds_object);
    for json in cases {
        assert_eq!(
            schema.validate(&json).is_ok(),
            serde_json::from_value::<PostgresHostConfig>(json.clone()).is_ok(),
            "{json}"
        );
    }
}
