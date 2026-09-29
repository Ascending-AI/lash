//! `lashctl finalize` and `lashctl finalize-hold` through the operator binary
//! (FIG-3800 B): the retirement check reads a Restate admin API's deployment
//! listing, here served by a stand-in that answers `GET /deployments` with
//! whatever the law registers, and the store is real PostgreSQL.
//!
//! This binary is the default build, whose writable range is `[1,1]`: its
//! finalize has no epoch to move, so these laws prove the preconditions and
//! the hold at the operator boundary, with their pinned exit codes and typed
//! refusals. The move of `F` itself, the fence and the backfills are proved
//! in `lash-postgres-store`'s finalize laws and in the rolling upgrade.

#![allow(clippy::disallowed_methods)]
#![expect(clippy::expect_used, reason = "integration-test setup and assertions")]

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};
use sqlx::{Connection, PgConnection};

/// A stand-in for the Restate admin API: `GET /deployments` answers the
/// listing the law last set.
struct Admin {
    url: String,
    listing: Arc<Mutex<Value>>,
}

impl Admin {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the admin stand-in");
        let url = format!("http://{}", listener.local_addr().expect("admin address"));
        let listing = Arc::new(Mutex::new(json!({"deployments": []})));
        let served = Arc::clone(&listing);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
                let mut request_line = String::new();
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    match reader.read_line(&mut header) {
                        Ok(0) | Err(_) => break,
                        Ok(_) if header == "\r\n" => break,
                        Ok(_) => {}
                    }
                }
                let (status, body) = if request_line.starts_with("GET /deployments ") {
                    ("200 OK", served.lock().expect("listing").to_string())
                } else {
                    ("404 Not Found", "{}".to_owned())
                };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Self { url, listing }
    }

    /// Register one deployment serving `service`.
    fn register(&self, id: &str, service: &str) {
        *self.listing.lock().expect("listing") = json!({"deployments": [{
            "id": id,
            "uri": format!("http://127.0.0.1:1/{id}"),
            "services": [{"name": service}],
        }]});
    }

    fn remove_all(&self) {
        *self.listing.lock().expect("listing") = json!({"deployments": []});
    }
}

fn run(args: &[&str], database_url: &str) -> (i32, Value) {
    let output = Command::new(env!("CARGO_BIN_EXE_lashctl"))
        .args(args)
        .arg("--json")
        .env("LASH_POSTGRES_DATABASE_URL", database_url)
        .output()
        .expect("run lashctl");
    let body: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "JSON output: {error}; stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    (output.status.code().expect("normal exit"), body)
}

/// A migrated scratch schema, its URL and an admin connection to drop it.
struct Scratch {
    url: String,
    schema: String,
    admin: PgConnection,
}

impl Scratch {
    async fn create() -> Option<Self> {
        let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") else {
            eprintln!("finalize laws need LASH_POSTGRES_DATABASE_URL");
            return None;
        };
        let schema = format!("lashctl_finalize_{}", uuid::Uuid::new_v4().simple());
        let mut admin = PgConnection::connect(&url).await.expect("connect admin");
        sqlx::query(&format!("CREATE SCHEMA {schema}"))
            .execute(&mut admin)
            .await
            .expect("create scratch schema");
        let separator = if url.contains('?') { '&' } else { '?' };
        let url = format!("{url}{separator}options=-csearch_path%3D{schema}");
        let (code, migrated) = run(&["migrate"], &url);
        assert_eq!(code, 0, "{migrated}");
        Some(Self { url, schema, admin })
    }

    async fn recorded_epoch(&self) -> i32 {
        let mut connection = PgConnection::connect(&self.url).await.expect("connect");
        let epoch = sqlx::query_scalar("SELECT format_version FROM lash_fleet_format")
            .fetch_one(&mut connection)
            .await
            .expect("read F");
        connection.close().await.expect("close");
        epoch
    }

    async fn execute(&self, sql: &str) {
        let mut connection = PgConnection::connect(&self.url).await.expect("connect");
        sqlx::query(sql)
            .execute(&mut connection)
            .await
            .expect("scratch statement");
        connection.close().await.expect("close");
    }

    async fn drop(mut self) {
        sqlx::query(&format!("DROP SCHEMA {} CASCADE", self.schema))
            .execute(&mut self.admin)
            .await
            .expect("drop scratch schema");
        self.admin.close().await.expect("close admin");
    }
}

const RETIRED: &str = "0123456789ab";

/// `lashctl finalize` refuses a generation that is not marked draining or
/// still holds work (exit 5, `generation_not_drained`), and one the engine
/// still holds a deployment for, in any namespace (exit 3,
/// `deployments_retained`). An engine it cannot read fails closed (exit 1).
/// Nothing moves until all three clear.
#[tokio::test]
async fn finalize_refuses_while_an_old_generation_is_live() {
    let Some(scratch) = Scratch::create().await else {
        return;
    };
    let admin = Admin::start();
    let finalize = [
        "finalize",
        RETIRED,
        "--restate-admin-url",
        admin.url.as_str(),
    ];

    let (code, unmarked) = run(&finalize, &scratch.url);
    assert_eq!(code, 5, "{unmarked}");
    assert_eq!(unmarked["command"], "finalize");
    assert_eq!(unmarked["result"], Value::Null);
    assert_eq!(unmarked["error"]["code"], "not_yet");
    assert_eq!(
        unmarked["error"]["refusal"]["refusal"],
        "generation_not_drained"
    );
    assert_eq!(
        unmarked["error"]["refusal"]["status"]["draining_since_ms"],
        Value::Null
    );

    let (code, _) = run(&["drain", RETIRED], &scratch.url);
    assert_eq!(code, 0);
    scratch
        .execute(&format!(
            "INSERT INTO lash_turn_parks (session_id, turn_id, park_id, reason_code, reason_json, \
             since_ms, last_refused_ms, attempts, park_build_generation) \
             VALUES ('s1', 't1', 1, 'test', '{{}}', 1, 1, 1, '{RETIRED}')"
        ))
        .await;
    let (code, parked) = run(&finalize, &scratch.url);
    assert_eq!(code, 5, "{parked}");
    assert_eq!(parked["error"]["refusal"]["status"]["parked_turns"], 1);
    scratch
        .execute("DELETE FROM lash_turn_parks WHERE session_id = 's1'")
        .await;

    admin.register(
        "dp_old",
        &format!("tenant-a.LashProcessWorkflow_g{RETIRED}"),
    );
    let (code, retained) = run(&finalize, &scratch.url);
    assert_eq!(code, 3, "{retained}");
    assert_eq!(retained["error"]["code"], "refused_precondition");
    assert_eq!(
        retained["error"]["refusal"],
        json!({
            "refusal": "deployments_retained",
            "generation": RETIRED,
            "deployments": [{"id": "dp_old", "uri": "http://127.0.0.1:1/dp_old"}],
        })
    );

    // Another generation's deployment does not hold this one.
    admin.register("dp_new", "LashProcessWorkflow_gfedcba987654");
    let unreachable = [
        "finalize",
        RETIRED,
        "--restate-admin-url",
        "http://127.0.0.1:1",
    ];
    let (code, closed) = run(&unreachable, &scratch.url);
    assert_eq!(code, 1, "{closed}");
    assert_eq!(closed["error"]["code"], "unexpected_failure");
    assert_eq!(scratch.recorded_epoch().await, 1);

    let (code, finalized) = run(&finalize, &scratch.url);
    assert_eq!(code, 0, "{finalized}");
    assert_eq!(
        finalized["result"],
        json!({
            "retired_generation": RETIRED,
            "flip": {"outcome": "already_finalized", "fleet": 1},
            "fleet_format": 1,
            "backfills": [],
        })
    );
    admin.remove_all();
    scratch.drop().await;
}

/// The hold refuses the automatic finalize (exit 3, `held`), `--override-hold`
/// finalizes by hand under it, and a cleared hold lets the automatic
/// finalize through. `finalize-hold` shows, sets and clears the one hold on
/// the fleet row.
#[tokio::test]
async fn finalize_honours_operator_hold() {
    let Some(scratch) = Scratch::create().await else {
        return;
    };
    let admin = Admin::start();
    let finalize = [
        "finalize",
        RETIRED,
        "--restate-admin-url",
        admin.url.as_str(),
    ];
    let (code, _) = run(&["drain", RETIRED], &scratch.url);
    assert_eq!(code, 0);

    let (code, shown) = run(&["finalize-hold", "show"], &scratch.url);
    assert_eq!(code, 0);
    assert_eq!(
        shown["result"],
        json!({"held": false, "reason": null, "held_at_ms": null})
    );
    let (code, set) = run(
        &["finalize-hold", "set", "--reason", "watch N+1 for a day"],
        &scratch.url,
    );
    assert_eq!(code, 0, "{set}");
    assert_eq!(set["result"]["held"], true);
    assert_eq!(set["result"]["reason"], "watch N+1 for a day");
    let held_at_ms = set["result"]["held_at_ms"].clone();
    assert!(held_at_ms.as_u64().is_some_and(|at| at > 0));

    let (code, held) = run(&finalize, &scratch.url);
    assert_eq!(code, 3, "{held}");
    assert_eq!(
        held["error"]["refusal"],
        json!({"refusal": "held", "hold": {"reason": "watch N+1 for a day", "held_at_ms": held_at_ms}})
    );
    assert!(
        held["error"]["message"]
            .as_str()
            .is_some_and(|message| message.contains("lashctl finalize-hold clear"))
    );

    let mut by_hand = finalize.to_vec();
    by_hand.push("--override-hold");
    let (code, overridden) = run(&by_hand, &scratch.url);
    assert_eq!(code, 0, "{overridden}");
    let (code, still) = run(&["finalize-hold", "show"], &scratch.url);
    assert_eq!(code, 0);
    assert_eq!(
        still["result"]["held"], true,
        "finalizing by hand keeps the hold"
    );

    let (code, cleared) = run(&["finalize-hold", "clear"], &scratch.url);
    assert_eq!(code, 0);
    assert_eq!(
        cleared["result"],
        json!({
            "held": false,
            "reason": null,
            "held_at_ms": null,
            "cleared": {"held": true, "reason": "watch N+1 for a day", "held_at_ms": held_at_ms},
        })
    );
    let (code, automatic) = run(&finalize, &scratch.url);
    assert_eq!(code, 0, "{automatic}");
    assert_eq!(scratch.recorded_epoch().await, 1);
    scratch.drop().await;
}
