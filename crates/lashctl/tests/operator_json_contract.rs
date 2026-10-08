//! The public JSON envelope and exit codes of the operator binary.

#![allow(clippy::disallowed_methods)]
#![expect(clippy::expect_used, reason = "integration-test setup and assertions")]

use std::process::{Command, Output};

use serde_json::{Value, json};

fn run(args: &[&str], database_url: Option<&str>) -> (i32, Value) {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lashctl"));
    command.args(args);
    command.env_remove("LASH_POSTGRES_DATABASE_URL");
    command.env_remove("LASH_SQLITE_PATH");
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
    assert_eq!(code, 0);
    assert_envelope(&version, "version", true, false);

    let (code, usage) = run(&["--json", "nonsense"], None);
    assert_eq!(code, 2);
    assert_envelope(&usage, "invalid", false, true);
    assert_eq!(usage["error"]["code"], "usage");

    let cases = [
        ("migrate", vec!["migrate", "--json"]),
        ("preflight", vec!["preflight", "--json"]),
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

    let (code, failed) = run(&["migrate", "--dry-run", "--json"], Some("invalid-url"));
    assert_eq!(code, 1);
    assert_envelope(&failed, "migrate", false, true);
    assert_eq!(failed["error"]["code"], "unexpected_failure");
}

#[test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
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

/// The PostgreSQL contract of the store verbs over a scratch schema: the
/// migration plan and its run, the empty backfill and contract phases, a
/// ready preflight whose stamp is the provisioned schema version and whose
/// fleet epoch `migrate` recorded, an expanded stamp that still admits this
/// reader, and a raised reader floor refused typed.
#[tokio::test]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
async fn operator_json_contract_postgres() {
    use sqlx::Connection as _;

    let url = lash_postgres_store::testing::required_database_url();
    let schema = format!("lashctl_{}", uuid::Uuid::new_v4().simple());
    let mut admin = sqlx::PgConnection::connect(&url)
        .await
        .expect("connect scratch admin");
    sqlx::query(&format!("CREATE SCHEMA {schema}"))
        .execute(&mut admin)
        .await
        .expect("create scratch schema");
    let separator = if url.contains('?') { '&' } else { '?' };
    let scratch_url = format!("{url}{separator}options=-csearch_path%3D{schema}");

    let (code, plan) = run(&["migrate", "--dry-run", "--json"], Some(&scratch_url));
    assert_eq!(code, 0, "{plan}");
    assert_envelope(&plan, "migrate", true, false);
    assert_eq!(plan["result"]["dry_run"], true);
    assert!(
        !plan["result"]["planned"]
            .as_array()
            .expect("plan")
            .is_empty()
    );

    let (code, migrated) = run(&["migrate", "--json"], Some(&scratch_url));
    assert_eq!(code, 0, "{migrated}");
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
    assert_eq!(code, 0, "{preflight}");
    assert_envelope(&preflight, "preflight", true, false);
    assert_eq!(preflight["result"]["outcome"], "ready");
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

    let mut scratch = sqlx::PgConnection::connect(&scratch_url)
        .await
        .expect("connect scratch catalog");
    sqlx::query(
        "UPDATE lash_schema_versions SET version = version + 1
         WHERE component = 'lash-postgres-store'",
    )
    .execute(&mut scratch)
    .await
    .expect("expand stamp under the old floor");
    let (code, expanded) = run(&["preflight", "--json"], Some(&scratch_url));
    assert_eq!(code, 0, "{expanded}");
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
    assert_eq!(code, 4, "{incompatible}");
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

/// The recovery verbs over a SQLite file the operator names: each answers
/// its JSON envelope, a stalled page resumes after its last row, a re-armed
/// obligation leaves the stalled list, re-arming one that is not stalled
/// answers `false`, the deployment status reports its admission, and an
/// obligation kind this build does not name is a usage refusal.
#[test]
fn recovery_json_uses_sqlite_and_keeps_typed_refusals() {
    let path = std::env::temp_dir()
        .join(format!("lashctl-recovery-{}", uuid::Uuid::new_v4()))
        .join("lash.db");
    let invoke = |args: &[&str]| {
        let output = Command::new(env!("CARGO_BIN_EXE_lashctl"))
            .args(args)
            .arg("--json")
            .arg("--sqlite-path")
            .arg(&path)
            .env_remove("LASH_POSTGRES_DATABASE_URL")
            .env_remove("LASH_SQLITE_PATH")
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
    const KIND: &str = "artifact_cleanup";
    let (code, empty) = invoke(&["stalled", "list", KIND, "--limit", "1"]);
    assert_eq!(code, 0, "{empty}");
    assert_envelope(&empty, "stalled-list", true, false);
    assert_eq!(empty["result"], json!({"records":[], "next":null}));
    let (code, absent) = invoke(&["stalled", "rearm", KIND, "absent"]);
    assert_eq!(code, 0, "{absent}");
    assert_envelope(&absent, "stalled-rearm", true, false);
    assert_eq!(absent["result"]["rearmed"], false);
    for admission in ["true", "false"] {
        let (code, status) = invoke(&["deployment-status", "--accepting-new-work", admission]);
        assert_eq!(code, 0, "{status}");
        assert_envelope(&status, "deployment-status", true, false);
        assert_eq!(status["result"]["accepting_new_work"], admission == "true");
    }
    let (code, unknown) = invoke(&["stalled", "list", "process_start"]);
    assert_eq!(code, 2, "{unknown}");
    assert_eq!(unknown["error"]["code"], "usage");

    // Two stalled cleanups, seeded through the store's own ports.
    let runtime = tokio::runtime::Runtime::new().expect("recovery store setup runtime");
    let ids = runtime.block_on(async {
        use lash::StoreSet as _;
        use lash_core_store::store::{
            ClaimToken, DeliveryError, ObligationKind, ObligationSettlement, StallReason,
        };
        let stores =
            lash::sqlite::SqliteStoreSet::open(&path, lash::sqlite::SqliteSynchronous::Normal)
                .await
                .expect("open the selected store");
        let ledger = stores.obligation_ledger(ObligationKind::ArtifactCleanup);
        let mut ids = Vec::new();
        for index in 0..2 {
            let id = stores
                .artifact_cleanup()
                .arm_cleanup(
                    &lash_core_store::artifact_referrer::ArtifactCleanup::ended(
                        lash_core_store::artifact_referrer::ArtifactReferrer::HostPin(
                            lash_core_store::artifact_referrer::HostArtifactPin::mint(),
                        ),
                        Vec::new(),
                        None,
                    ),
                    1,
                )
                .await
                .expect("arm a cleanup");
            let token = ClaimToken::new(format!("recovery-wire-{index}"));
            ledger
                .claim(&id, &token, 1, 1000)
                .await
                .expect("claim the cleanup")
                .expect("the cleanup is due");
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
                .expect("stall the cleanup");
            ids.push(id.to_string());
        }
        ids.sort();
        ids
    });
    let (code, first) = invoke(&["stalled", "list", KIND, "--limit", "1"]);
    assert_eq!(code, 0, "{first}");
    assert_eq!(first["result"]["records"][0]["obligation_id"], ids[0]);
    assert_eq!(first["result"]["records"][0]["reason"], "refused");
    assert_eq!(first["result"]["next"], ids[0]);
    let (code, second) = invoke(&["stalled", "list", KIND, "--limit", "1", "--after", &ids[0]]);
    assert_eq!(code, 0, "{second}");
    assert_eq!(second["result"]["records"][0]["obligation_id"], ids[1]);
    assert_eq!(second["result"]["next"], Value::Null);
    let (code, rearmed) = invoke(&["stalled", "rearm", KIND, &ids[0]]);
    assert_eq!(code, 0, "{rearmed}");
    assert_eq!(rearmed["result"]["rearmed"], true);
    let (code, remaining) = invoke(&["stalled", "list", KIND]);
    assert_eq!(code, 0, "{remaining}");
    assert_eq!(
        remaining["result"]["records"]
            .as_array()
            .expect("records")
            .len(),
        1
    );
    assert_eq!(remaining["result"]["records"][0]["obligation_id"], ids[1]);
    std::fs::remove_dir_all(path.parent().expect("the store's directory"))
        .expect("remove recovery store");
}
