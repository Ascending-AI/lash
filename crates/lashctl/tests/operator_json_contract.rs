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
    assert_eq!(
        version["result"]["fleet_writable"],
        json!({"min":1,"max":1})
    );
    assert_eq!(
        version["result"]["components"]
            .as_array()
            .expect("components")
            .len(),
        7
    );
    assert_eq!(
        version["result"]["wires"]["restate"],
        json!({"min":1,"max":1})
    );
    assert_keys(
        &version["result"],
        &[
            "components",
            "fleet_writable",
            "generation",
            "release",
            "wires",
        ],
    );

    let (code, usage) = run(&["--json", "nonsense"], None);
    assert_eq!(code, 2);
    assert_envelope(&usage, "invalid", false, true);
    assert_eq!(usage["error"]["code"], "usage");

    let cases = [
        ("migrate", vec!["migrate", "--json"]),
        ("drain", vec!["drain", "0123456789ab", "--json"]),
        (
            "drain-status",
            vec!["drain-status", "0123456789ab", "--json"],
        ),
        ("end-drain", vec!["end-drain", "0123456789ab", "--json"]),
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

    let (code, refused) = run(
        &["migrate", "--phase", "contract", "--json"],
        Some("postgres://unused/unused"),
    );
    assert_eq!(code, 3);
    assert_envelope(&refused, "migrate", false, true);
    assert_eq!(refused["error"]["code"], "refused_precondition");

    let (code, failed) = run(&["migrate", "--dry-run", "--json"], Some("invalid-url"));
    assert_eq!(code, 1);
    assert_envelope(&failed, "migrate", false, true);
    assert_eq!(failed["error"]["code"], "unexpected_failure");
}

#[cfg(feature = "synthetic-next")]
#[test]
fn synthetic_next_version_reports_next_generation() {
    let (code, version) = run(&["version", "--json"], None);
    assert_eq!(code, 0);
    assert_envelope(&version, "version", true, false);
    assert_eq!(lash_restate::JOURNAL_LOGIC_EPOCH, 2);
    assert_eq!(
        version["result"]["generation"],
        lash::formats::build_generation().as_str()
    );
    assert_ne!(version["result"]["generation"], "21c7af909642");
}

#[tokio::test]
async fn operator_json_contract_postgres() {
    let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
        eprintln!("PostgreSQL leg needs LASH_POSTGRES_DATABASE_URL");
        return;
    };
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

    let (code, preflight) = run(&["preflight", "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&preflight, "preflight", true, false);
    assert_eq!(preflight["result"]["outcome"], "done");
    assert_keys(
        &preflight["result"],
        &["databases", "fleet_format", "outcome", "release"],
    );

    let generation = "0123456789ab";
    let (code, before) = run(&["drain-status", generation, "--json"], Some(&scratch_url));
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
            "stalled_obligations",
        ],
    );

    let (code, marked) = run(&["drain", generation, "--json"], Some(&scratch_url));
    assert_eq!(code, 0);
    assert_envelope(&marked, "drain", true, false);
    assert_eq!(
        marked["result"],
        json!({"generation":generation,"marked":true})
    );

    let (code, drained) = run(&["drain-status", generation, "--json"], Some(&scratch_url));
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

    let mut scratch = PgConnection::connect(&scratch_url)
        .await
        .expect("connect scratch catalog");
    sqlx::query("UPDATE lash_schema_versions SET version = version + 1 WHERE component = 'lash-postgres-store'")
        .execute(&mut scratch).await.expect("advance stamp beyond this build");
    scratch.close().await.expect("close scratch catalog");
    let (code, incompatible) = run(&["preflight", "--json"], Some(&scratch_url));
    assert_eq!(code, 4);
    assert_envelope(&incompatible, "preflight", true, true);
    assert_eq!(incompatible["error"]["code"], "incompatible_store");
    assert_eq!(incompatible["result"]["outcome"], "incompatible_store");

    sqlx::query(&format!("DROP SCHEMA {schema} CASCADE"))
        .execute(&mut admin)
        .await
        .expect("drop scratch schema");
    admin.close().await.expect("close scratch admin");
}
