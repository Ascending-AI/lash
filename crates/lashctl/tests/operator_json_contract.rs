//! The public JSON envelope and exit codes of the operator binary.

#![allow(clippy::disallowed_methods)]
#![expect(clippy::expect_used, reason = "integration-test setup and assertions")]

use std::process::{Command, Output};

use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};

fn assert_keys(value: &Value, expected: &[&str]) {
    let keys = value
        .as_object()
        .expect("object")
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(keys, expected);
}

fn run(args: &[&str], database_url: Option<&str>) -> (i32, Value) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lashctl"));
    command.args(args);
    command.env_remove("LASH_POSTGRES_DATABASE_URL");
    // `drain` hands over through the deployment's engine; the catalogs these
    // contracts drain hold no work for it to reach.
    command
        .env_remove("LASH_SQLITE_DIR")
        .env("RESTATE_AUTHORITY_ID", "lashctl-contract-test")
        .env("RESTATE_NAMESPACE", "lashctl-contract-test")
        .env("RESTATE_INGRESS_URL", "http://127.0.0.1:1")
        .env("RESTATE_ADMIN_URL", "http://127.0.0.1:1");
    if let Some(url) = database_url {
        command.env("LASH_POSTGRES_DATABASE_URL", url);
    }
    let output: Output = command.output().expect("run lashctl");
    let code = output.status.code().expect("normal exit");
    let body = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "JSON output: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (code, body)
}

/// The URL of a Restate admin API that serves an empty deployment fleet.
fn empty_admin() -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the admin stand-in");
    let url = format!("http://{}", listener.local_addr().expect("admin address"));
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
            let mut length = 0_usize;
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if line == "\r\n" => break,
                    Ok(_) => {
                        if let Some(value) =
                            line.to_ascii_lowercase().strip_prefix("content-length:")
                        {
                            length = value.trim().parse().unwrap_or(0);
                        }
                    }
                }
            }
            let mut request = vec![0_u8; length];
            let _ = reader.read_exact(&mut request);
            let body = json!({"deployments": []}).to_string();
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    url
}

fn assert_envelope(body: &Value, command: &str, has_result: bool, has_error: bool) {
    let object = body.as_object().expect("object");
    assert_eq!(object.len(), 4);
    assert_eq!(body["schema_version"], 1);
    assert_eq!(body["command"], command);
    assert_eq!(!body["result"].is_null(), has_result);
    assert_eq!(!body["error"].is_null(), has_error);
}

#[test]
fn operator_json_contract() {
    let (code, version) = run(&["version", "--json"], None);
    assert_eq!(code, 3);
    assert_envelope(&version, "version", false, true);

    let (code, usage) = run(&["--json", "nonsense"], None);
    assert_eq!(code, 2);
    assert_envelope(&usage, "invalid", false, true);
    assert_eq!(usage["error"]["code"], "usage");

    let cases = [
        ("migrate", vec!["migrate", "--json"]),
        ("drain", vec!["drain", "0123456789ab", "--json"]),
        (
            "drain-status",
            vec![
                "drain-status",
                "0123456789ab",
                "--restate-admin-url",
                "http://127.0.0.1:1",
                "--json",
            ],
        ),
        ("end-drain", vec!["end-drain", "0123456789ab", "--json"]),
        (
            "finalize",
            vec![
                "finalize",
                "0123456789ab",
                "--restate-admin-url",
                "http://127.0.0.1:1",
                "--json",
            ],
        ),
        ("finalize-hold", vec!["finalize-hold", "show", "--json"]),
        ("preflight", vec!["preflight", "--json"]),
        ("version", vec!["version", "--json"]),
    ];
    for (name, args) in cases {
        let (code, body) = run(&args, None);
        assert_eq!(code, 3, "{name}");
        assert_envelope(&body, name, false, true);
        assert_eq!(
            body["error"],
            json!({"code":"refused_precondition","message":"LASH_POSTGRES_DATABASE_URL must name the PostgreSQL database","refusal":null})
        );
    }

    // Finalize reads retirement from the engine, and a drain's status the
    // group children the engine still owes (FIG-4454), so both name it.
    for args in [
        vec!["finalize", "0123456789ab", "--json"],
        vec!["drain-status", "0123456789ab", "--json"],
        vec![
            "finalize",
            "not-a-generation",
            "--restate-admin-url",
            "http://x",
            "--json",
        ],
        vec!["finalize-hold", "set", "--json"],
        vec!["finalize-hold", "set", "--reason", " ", "--json"],
    ] {
        let (code, usage) = run(&args, Some("postgres://unused/unused"));
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(usage["error"]["code"], "usage", "{args:?}");
    }

    // The object commands read the engine, not the store: they name the
    // engine, a sweep names where it calls each object's `upgrade`, and an
    // engine that cannot be reached is a failure, not a verdict.
    for args in [
        vec!["objects-preflight", "--json"],
        vec!["objects-sweep", "--restate-admin-url", "http://x", "--json"],
        vec![
            "objects-preflight",
            "--restate-admin-url",
            "http://x",
            "--restate-ingress-url",
            "http://x",
            "--json",
        ],
        vec![
            "objects-preflight",
            "--restate-admin-url",
            "http://x",
            "--restate-admin-url",
            "http://y",
            "--json",
        ],
    ] {
        let (code, usage) = run(&args, None);
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(usage["error"]["code"], "usage", "{args:?}");
    }
    for (name, args) in [
        (
            "objects-preflight",
            vec![
                "objects-preflight",
                "--restate-admin-url",
                "http://127.0.0.1:1",
                "--json",
            ],
        ),
        (
            "objects-sweep",
            vec![
                "objects-sweep",
                "--restate-admin-url",
                "http://127.0.0.1:1",
                "--restate-ingress-url",
                "http://127.0.0.1:1",
                "--json",
            ],
        ),
    ] {
        let (code, failed) = run(&args, None);
        assert_eq!(code, 1, "{name}");
        assert_envelope(&failed, name, false, true);
        assert_eq!(failed["error"]["code"], "unexpected_failure", "{name}");
    }

    let (code, failed) = run(&["migrate", "--dry-run", "--json"], Some("invalid-url"));
    assert_eq!(code, 1);
    assert_envelope(&failed, "migrate", false, true);
    assert_eq!(failed["error"]["code"], "unexpected_failure");
}

#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn operator_json_contract_postgres() {
    let url = lash_postgres_store::testing::required_database_url();
    let schema = format!("lashctl_{}", uuid::Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&url)
        .await
        .expect("connect scratch admin");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut admin)
        .await
        .expect("create scratch schema");
    let separator = if url.contains('?') { '&' } else { '?' };
    let scratch_url = format!("{url}{separator}options=-csearch_path%3D{schema}");

    let (code, plan) = run(&["migrate", "--dry-run", "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&plan, "migrate", true, false);
    assert_eq!(plan["result"]["dry_run"], true);
    assert_keys(
        &plan["result"],
        &[
            "applied",
            "dry_run",
            "executed",
            "found_version",
            "namespace",
            "planned",
        ],
    );
    assert!(
        !plan["result"]["planned"]
            .as_array()
            .expect("plan")
            .is_empty()
    );

    let (code, migrated) = run(&["migrate", "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&migrated, "migrate", true, false);
    assert_eq!(migrated["result"]["dry_run"], false);
    assert!(
        !migrated["result"]["executed"]
            .as_array()
            .expect("steps")
            .is_empty()
    );

    // The 1.0 release carries no backfill and no contract step: both phases
    // run and find nothing to do.
    for phase in ["backfill", "contract"] {
        for dry_run in [true, false] {
            let mut args = vec!["migrate", "--phase", phase, "--json"];
            if dry_run {
                args.push("--dry-run");
            }
            let (code, body) = run(&args, Some(&scratch_url));
            assert_eq!(code, 0, "{args:?}: {body}");
            assert_envelope(&body, "migrate", true, false);
            assert_eq!(body["result"]["executed"], json!([]));
            assert_eq!(body["result"]["planned"], json!([]));
        }
    }

    let (code, preflight) = run(&["preflight", "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&preflight, "preflight", true, false);
    assert_eq!(preflight["result"]["outcome"], "ready");
    assert_keys(
        &preflight["result"],
        &["databases", "fleet_format", "outcome", "release"],
    );
    // The component version `schema.sql` provisions is also its reader floor.
    let provisioned = lash_postgres_store::PostgresStorage::schema_version();
    assert_eq!(preflight["result"]["databases"][0]["expected"], provisioned);
    assert_eq!(
        preflight["result"]["databases"][0]["min_reader"],
        provisioned
    );
    assert_eq!(preflight["result"]["databases"][0]["verdict"], "matches");
    // `migrate` seeded the fleet epoch before any worker opened the store
    // (FIG-4075), so no first opener decides it.
    assert_eq!(
        preflight["result"]["fleet_format"],
        json!({"state":"recorded","version":1})
    );

    let (code, empty_version) = run(&["version", "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&empty_version, "version", true, false);
    assert_eq!(empty_version["result"]["fleet_generations"], json!([]));
    assert_eq!(
        empty_version["result"]["fleet_writable"],
        json!({"min":1,"max":1})
    );
    assert_eq!(
        empty_version["result"]["components"]
            .as_array()
            .expect("components")
            .len(),
        5
    );
    assert_eq!(
        empty_version["result"]["wires"]["restate"],
        json!({"min":1,"max":1})
    );
    assert_keys(
        &empty_version["result"],
        &[
            "components",
            "fleet_generations",
            "fleet_writable",
            "release",
            "wires",
        ],
    );
    #[cfg(feature = "synthetic-next")]
    {
        assert_eq!(lash_restate::JOURNAL_LOGIC_EPOCH, 2);
    }

    let generation = "0123456789ab";
    let admin_url = empty_admin();
    let drain_status = [
        "drain-status",
        generation,
        "--restate-admin-url",
        admin_url.as_str(),
        "--json",
    ];
    let (code, before) = run(&drain_status, Some(&scratch_url));
    assert_eq!(code, 5);
    assert_envelope(&before, "drain-status", true, true);
    assert_eq!(before["error"]["code"], "not_yet");
    assert_eq!(before["result"]["drained"], false);
    assert_keys(
        &before["result"],
        &[
            "checked_at_ms",
            "closing_sessions",
            "drained",
            "draining_since_ms",
            "generation",
            "in_flight_turns",
            "live_processes",
            "parked_processes",
            "parked_turns",
            "stalled",
            "stalled_obligations",
        ],
    );
    assert_eq!(before["result"]["stalled"], json!([]));

    let (code, marked) = run(&["drain", generation, "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&marked, "drain", true, false);
    assert_eq!(
        marked["result"],
        json!({"generation":generation,"marked":true})
    );

    let mut scratch = PgConnection::connect(&scratch_url)
        .await
        .expect("connect scratch catalog");
    sqlx::query("INSERT INTO lash_turn_parks (session_id, turn_id, park_id, reason_code, reason_json, since_ms, last_refused_ms, attempts, park_build_generation) VALUES ('s1', 't1', 1, 'test', '{}', 1, 1, 1, 'fedcba987654')")
        .execute(&mut scratch)
        .await
        .expect("insert pinned park");
    let (code, version) = run(&["version", "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&version, "version", true, false);
    assert_eq!(
        version["result"]["fleet_generations"],
        json!([
            {"generation":generation,"draining":true,"source":"postgres"},
            {"generation":"fedcba987654","draining":false,"source":"postgres"},
        ])
    );
    assert!(version["result"].get("generation").is_none());

    let human = Command::new(env!("CARGO_BIN_EXE_lashctl"))
        .arg("version")
        .env("LASH_POSTGRES_DATABASE_URL", &scratch_url)
        .output()
        .expect("human version");
    assert!(human.status.success());
    let text = String::from_utf8(human.stdout).expect("human output");
    assert!(text.contains("0123456789ab (draining: true, source: postgres)"));
    assert!(text.contains("fedcba987654 (draining: false, source: postgres)"));

    let (code, drained) = run(&drain_status, Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&drained, "drain-status", true, false);
    assert_eq!(drained["result"]["drained"], true);

    let (code, ended) = run(&["end-drain", generation, "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&ended, "end-drain", true, false);
    assert_eq!(
        ended["result"],
        json!({"generation":generation,"cleared":true})
    );

    sqlx::query(
        "UPDATE lash_schema_versions SET version = version + 1
         WHERE component = 'lash-postgres-store'",
    )
    .execute(&mut scratch)
    .await
    .expect("expand stamp under the old floor");
    let (code, expanded) = run(&["preflight", "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_eq!(expanded["result"]["databases"][0]["verdict"], "expanded");
    assert_eq!(expanded["result"]["databases"][0]["found"], provisioned + 1);
    assert_eq!(
        expanded["result"]["databases"][0]["min_reader"],
        provisioned
    );

    sqlx::query(
        "UPDATE lash_schema_versions SET min_reader = version
         WHERE component = 'lash-postgres-store'",
    )
    .execute(&mut scratch)
    .await
    .expect("raise the reader floor");
    scratch.close().await.expect("close scratch catalog");
    let (code, incompatible) = run(&["preflight", "--json"], Some(&scratch_url));
    assert_eq!(code, 4);
    assert_envelope(&incompatible, "preflight", true, true);
    assert_eq!(incompatible["error"]["code"], "incompatible_store");
    assert_eq!(incompatible["result"]["outcome"], "refused");
    assert_eq!(incompatible["result"]["databases"][0]["verdict"], "refused");
    assert_eq!(
        incompatible["result"]["databases"][0]["min_reader"],
        provisioned + 1
    );
    assert_eq!(
        incompatible["result"]["databases"][0]["refusal"]["refusal"],
        "reader_floor_above"
    );

    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut admin)
        .await
        .expect("drop scratch schema");
    admin.close().await.expect("close scratch admin");
}

#[test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
fn rolling_preflight_refuses_an_oversubscribed_budget() {
    let url = lash_postgres_store::testing::required_database_url();
    let (code, body) = run(
        &[
            "preflight",
            "--processes-per-generation",
            "1000000",
            "--pool-max",
            "16",
            "--generations",
            "3",
            "--workers",
            "12",
            "--admin-headroom",
            "10",
            "--json",
        ],
        Some(&url),
    );
    assert_eq!(code, 3, "capacity refusal: {body}");
    assert_eq!(
        body["error"]["refusal"]["refusal"],
        "connection_budget_exceeded"
    );
    let report = &body["error"]["refusal"]["report"];
    assert_eq!(report["peak_connections"], 48000022u64);
    assert!(
        report["server"]["max_connections"]
            .as_u64()
            .expect("observed capacity")
            < 48000022
    );
    let (code, accepted) = run(
        &[
            "preflight",
            "--processes-per-generation",
            "1",
            "--pool-max",
            "1",
            "--generations",
            "2",
            "--workers",
            "0",
            "--admin-headroom",
            "10",
            "--json",
        ],
        Some(&url),
    );
    assert_eq!(code, 0, "small declaration fits: {accepted}");
    assert_eq!(
        accepted["result"]["connection_budget"]["peak_connections"],
        12
    );
    assert_eq!(
        accepted["result"]["connection_budget"]["server"],
        report["server"]
    );
    println!(
        "oversubscribed preflight refused; bounded roll accepted against live server capacity"
    );
}

/// FIG-5037: the operator CLI addresses the selected SQLite store, preserves
/// resumable feed cursors, and returns typed park refusals in its JSON envelope.
#[test]
fn recovery_json_uses_sqlite_and_keeps_typed_refusals() {
    let path = std::env::temp_dir().join(format!("lashctl-recovery-{}", uuid::Uuid::new_v4()));
    let invoke = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_lashctl"))
            .args(args)
            .arg("--json")
            .arg("--sqlite-dir")
            .arg(&path)
            .env_remove("LASH_POSTGRES_DATABASE_URL")
            .env_remove("LASH_SQLITE_DIR")
            .env("RESTATE_AUTHORITY_ID", "lashctl-recovery-test")
            .env("RESTATE_NAMESPACE", "lashctl-recovery-test")
            .env("RESTATE_INGRESS_URL", "http://127.0.0.1:1")
            .env("RESTATE_ADMIN_URL", "http://127.0.0.1:1")
            .output()
            .expect("run recovery CLI");
        let body: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "recovery JSON: {error}; stderr: {}",
                String::from_utf8_lossy(&output.stderr)
            )
        });
        (output.status.code().expect("normal exit"), body)
    };
    let (code, parks) = invoke(&["park", "list", "--limit", "1"]);
    assert_eq!(code, 0, "{parks}");
    assert_envelope(&parks, "park-list", true, false);
    assert_eq!(parks["result"], json!({"records":[], "next":null}));
    let (code, events) = invoke(&["park", "events"]);
    assert_eq!(code, 0, "{events}");
    let cursor = events["result"]["next"].to_string();
    let (code, resumed) = invoke(&["park", "events", "--after", &cursor]);
    assert_eq!(code, 0, "{resumed}");
    assert_eq!(resumed["result"], events["result"]);
    for kind in lash_core_store::store::ObligationKind::ALL {
        let (code, page) = invoke(&["stalled", "list", kind.label(), "--limit", "1"]);
        assert_eq!(code, 0, "{page}");
        assert_envelope(&page, "stalled-list", true, false);
        assert_eq!(page["result"], json!({"records":[], "next":null}));
        let (code, rearm) = invoke(&["stalled", "rearm", kind.label(), "absent"]);
        assert_eq!(code, 0, "{rearm}");
        assert_eq!(rearm["result"]["rearmed"], false);
    }
    for admission in ["true", "false"] {
        let (code, status) = invoke(&["deployment-status", "--accepting-new-work", admission]);
        assert_eq!(code, 0, "{status}");
        assert_eq!(status["result"]["accepting_new_work"], admission == "true");
        assert_eq!(status["result"]["drained"], admission == "false");
    }
    let target =
        json!({"kind":"process", "process_id":lash::ProcessId::fixture("absent")}).to_string();
    let (code, refused) = invoke(&["park", "fork", "--target", &target, "--park-id", "1"]);
    assert_eq!(code, 3, "{refused}");
    assert_envelope(&refused, "park-fork", false, true);
    assert_eq!(
        refused["error"]["refusal"],
        json!({"kind":"fork_requires_turn"})
    );
    let (code, refused) = invoke(&["park", "redrive", "--target", &target, "--park-id", "1"]);
    assert_eq!(code, 3, "{refused}");
    assert_envelope(&refused, "park-redrive", false, true);
    assert_eq!(refused["error"]["refusal"]["kind"], "engine_refused");
    assert_eq!(refused["error"]["refusal"]["code"], "process_not_visible");
    // The wire pages must resume after the last returned row, including when
    // there is another row behind a one-record page. Seed through store APIs.
    let runtime = tokio::runtime::Runtime::new().expect("recovery store setup runtime");
    let ids = runtime.block_on(async {
        use lash::StoreSet;
        use lash::process::{
            ProcessExecutionEnvRef, ProcessExecutionWriteAuthority, ProcessInput, ProcessLifecycle,
            ProcessProvenance, ProcessRegistrar, ProcessRegistration,
        };
        use lash_core_store::store::{
            ClaimToken, DeliveryError, ObligationKey, ObligationKind, ObligationSettlement,
            ParkReason, StallReason,
        };
        let stores = lash::sqlite::SqliteStoreSet::open(&path)
            .await
            .expect("seed selected store");
        let registry = stores.process_registry();
        let ledger = stores.obligation_ledger(ObligationKind::ProcessStart);
        let mut stalled = Vec::new();
        for index in 0..2 {
            let process = registry
                .register_process(
                    ProcessRegistration::new(
                        ProcessInput::Engine {
                            kind: "recovery-wire-test".into(),
                            payload: Value::Null,
                        },
                        ProcessProvenance::host(),
                        lash::process::Lifetime::Detached,
                    )
                    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(
                        "process-env:recovery-wire-test",
                    ))),
                )
                .await
                .expect("register process")
                .id;
            let id = ObligationKey::ProcessStart {
                process_id: process.clone(),
            }
            .id();
            let token = ClaimToken::new(format!("recovery-wire-{index}"));
            ledger
                .claim(&id, &token, 1, 1000)
                .await
                .expect("claim start")
                .expect("start owed");
            ledger
                .settle(
                    &id,
                    &token,
                    ObligationSettlement::Stall {
                        reason: StallReason::Refused,
                        error: DeliveryError::new(
                            lash_core_execution::RuntimeErrorCode::StoreRefused,
                            "operator repair needed",
                        ),
                    },
                    2,
                )
                .await
                .expect("stall start");
            stalled.push(id.to_string());
            let process = registry
                .register_process(
                    ProcessRegistration::new(
                        ProcessInput::Engine {
                            kind: "recovery-wire-test".into(),
                            payload: Value::Null,
                        },
                        ProcessProvenance::host(),
                        lash::process::Lifetime::Detached,
                    )
                    .with_execution_env_ref(Some(ProcessExecutionEnvRef::new(
                        "process-env:recovery-wire-test",
                    ))),
                )
                .await
                .expect("register park owner")
                .id;
            let authority = ProcessExecutionWriteAuthority::invocation(
                process.clone(),
                format!("recovery-wire-{index}"),
            )
            .bind_attempt(1);
            registry
                .record_first_started_with_authority(
                    &process,
                    authority.invocation_started().expect("bound authority"),
                    &authority,
                )
                .await
                .expect("record started process");
            registry
                .park_process_with_authority(
                    &process,
                    ParkReason::engine_retry_exhausted(1, None, "engine retry stopped".into())
                        .into(),
                    &authority,
                )
                .await
                .expect("park process");
        }
        stalled.sort();
        stalled
    });
    let (code, first) = invoke(&["park", "list", "--limit", "1"]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(
        first["result"]["records"]
            .as_array()
            .expect("records")
            .len(),
        1
    );
    let cursor = first["result"]["next"].to_string();
    assert_ne!(cursor, "null");
    let (code, second) = invoke(&["park", "list", "--limit", "1", "--after", &cursor]);
    assert_eq!(code, 0, "{second}");
    assert_ne!(
        first["result"]["records"][0]["target"],
        second["result"]["records"][0]["target"]
    );
    assert_eq!(second["result"]["next"], Value::Null);
    let (code, first) = invoke(&["stalled", "list", "process_start", "--limit", "1"]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["result"]["records"][0]["obligation_id"], ids[0]);
    assert_eq!(first["result"]["next"], ids[0]);
    let (code, second) = invoke(&[
        "stalled",
        "list",
        "process_start",
        "--limit",
        "1",
        "--after",
        &ids[0],
    ]);
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["result"]["records"][0]["obligation_id"], ids[1]);
    assert_eq!(second["result"]["next"], Value::Null);
    let (code, rearmed) = invoke(&["stalled", "rearm", "process_start", &ids[0]]);
    assert_eq!(code, 0, "{rearmed}");
    assert_eq!(rearmed["result"]["rearmed"], true);
    let (code, remaining) = invoke(&["stalled", "list", "process_start"]);
    assert_eq!(code, 0, "{remaining}");
    assert_eq!(
        remaining["result"]["records"]
            .as_array()
            .expect("records")
            .len(),
        1
    );
    assert_eq!(remaining["result"]["records"][0]["obligation_id"], ids[1]);
    std::fs::remove_dir_all(path).expect("remove recovery store");
}
