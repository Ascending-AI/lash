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
    assert_eq!(code, 3);
    assert_envelope(&version, "version", false, true);

    let (code, usage) = run(&["--json", "nonsense"], None);
    assert_eq!(code, 2);
    assert_envelope(&usage, "invalid", false, true);
    assert_eq!(usage["error"]["code"], "usage");

    let cases = [
        ("migrate", vec!["migrate", "--json"]),
        ("drain", vec!["drain", "0123456789ab", "--json"]),
        ("end-drain", vec!["end-drain", "0123456789ab", "--json"]),
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

    for args in [
        vec!["finalize-hold", "set", "--json"],
        vec!["finalize-hold", "set", "--reason", " ", "--json"],
    ] {
        let (code, usage) = run(&args, Some("postgres://unused/unused"));
        assert_eq!(code, 2, "{args:?}");
        assert_eq!(usage["error"]["code"], "usage", "{args:?}");
    }

    let (code, failed) = run(&["migrate", "--dry-run", "--json"], Some("invalid-url"));
    assert_eq!(code, 1);
    assert_envelope(&failed, "migrate", false, true);
    assert_eq!(failed["error"]["code"], "unexpected_failure");
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
