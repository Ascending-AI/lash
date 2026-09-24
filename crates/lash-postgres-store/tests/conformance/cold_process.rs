//! The Postgres tier's cold-process laws: a killed helper process leaves
//! durable state its successor reopens and settles.

use super::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_host_satisfies_cold_process_await_event_conformance_when_configured() {
    use tokio::io::{AsyncBufReadExt as _, BufReader};
    use tokio::process::Command;

    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-process AwaitEvent conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    for identity in ["tool_completion", "turn_cancel_gate"] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new(lash_conformance::helper_executable(
            "postgres-await-event-helper",
        ))
        .arg(identity)
        .arg(&nonce)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn cold-process helper for {identity}: {error}"));
        let stdout = child.stdout.take().expect("helper stdout pipe");
        let mut lines = BufReader::new(stdout).lines();
        let encoded_key =
            tokio::time::timeout(std::time::Duration::from_secs(30), lines.next_line())
                .await
                .unwrap_or_else(|_| panic!("helper did not mint {identity} key"))
                .expect("read helper key")
                .unwrap_or_else(|| panic!("helper exited before printing {identity} key"));
        let key: AwaitEventKey = serde_json::from_str(&encoded_key)
            .unwrap_or_else(|error| panic!("decode helper {identity} key: {error}"));

        child
            .kill()
            .await
            .unwrap_or_else(|error| panic!("kill parked {identity} helper: {error}"));
        let status = child
            .wait()
            .await
            .unwrap_or_else(|error| panic!("reap parked {identity} helper: {error}"));
        assert!(
            !status.success(),
            "killed {identity} helper exited successfully"
        );

        let resolver = Arc::new(
            PostgresStorage::connect(&database_url().expect("configured Postgres database URL"))
                .await
                .expect("cold-process resolver")
                .effect_host(),
        );
        let terminal = if identity == "turn_cancel_gate" {
            let address = lash_core_execution::runtime::TurnAddress::new(
                format!("cold-process-{nonce}-session"),
                format!("cold-process-{nonce}-turn"),
            );
            let store_factory: Arc<dyn SessionStoreFactory> =
                Arc::new(storage.session_store_factory());
            store_factory
                .create_store(&lash_core_execution::SessionStoreCreateRequest {
                    pending_observer_intents: Vec::new(),
                    session_id: address.session_id.clone(),
                    relation: lash_core_execution::SessionRelation::Root,
                    policy: lash_core_execution::SessionPolicy::new(
                        lash_core_execution::TurnBudget::Unbounded,
                    ),
                })
                .await
                .expect("create cold-process cancellation session");
            let receipt = lash_core_execution::runtime::TurnWorkDriver::for_catalog(
                Arc::clone(&resolver) as Arc<dyn EffectHost>,
                store_factory,
            )
            .request_cancel(lash_core_execution::runtime::TurnCancelRequest::new(
                address,
                format!("cold-process-{nonce}-cancel"),
                None,
            ))
            .await
            .expect("request cancellation through a successor owner");
            assert!(matches!(
                receipt.outcome,
                lash_core_execution::runtime::TurnCancelOutcome::Requested(_)
            ));
            resolver
                .peek_await_event(&key)
                .await
                .expect("peek successor cancellation")
                .expect("successor cancellation resolves the killed owner's gate")
        } else {
            let terminal = Resolution::Ok(serde_json::json!({
                "cold_process": true,
                "identity": identity,
                "nonce": nonce,
            }));
            assert_eq!(
                resolver
                    .resolve_await_event(&key, terminal.clone())
                    .await
                    .unwrap_or_else(|error| panic!(
                        "resolve killed-helper {identity} key: {error}"
                    )),
                ResolveOutcome::Accepted
            );
            terminal
        };
        drop(resolver);

        let observer =
            PostgresStorage::connect(&database_url().expect("configured Postgres database URL"))
                .await
                .expect("cold-process observer")
                .effect_host();
        assert_eq!(
            observer
                .peek_await_event(&key)
                .await
                .unwrap_or_else(|error| panic!("peek killed-helper {identity} key: {error}")),
            Some(terminal.clone())
        );
        assert_eq!(
            observer
                .await_await_event(&key, tokio_util::sync::CancellationToken::new(), None)
                .await
                .unwrap_or_else(|error| panic!("observe killed-helper {identity} key: {error}")),
            terminal
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn postgres_effect_replay_satisfies_cold_process_crash_conformance_when_configured() {
    use tokio::process::Command;

    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping Postgres cold-process effect replay conformance: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let dir = tempfile::tempdir().expect("cold-process effect replay tempdir");
    let marker = dir.path().join("external-effect.log");
    let nonce = uuid::Uuid::new_v4().to_string();
    let run = |action: &'static str| {
        let marker = marker.clone();
        let nonce = nonce.clone();
        async move {
            tokio::time::timeout(
                std::time::Duration::from_secs(30),
                Command::new(lash_conformance::helper_executable(
                    "postgres-await-event-helper",
                ))
                .arg(action)
                .arg(nonce)
                .arg(marker)
                .output(),
            )
            .await
            .unwrap_or_else(|_| panic!("{action} helper timed out"))
            .unwrap_or_else(|error| panic!("spawn {action} helper: {error}"))
        }
    };

    let crashed = run("effect_crash").await;
    assert_eq!(crashed.status.code(), Some(86));
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read crashed effect marker")
            .lines()
            .count(),
        1
    );

    let completed = run("effect_complete").await;
    assert!(
        completed.status.success(),
        "successor helper failed: {}",
        String::from_utf8_lossy(&completed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read re-executed effect marker")
            .lines()
            .count(),
        2,
        "at-least-once means re-execution before outcome recording"
    );

    let replayed = run("effect_replay").await;
    assert!(
        replayed.status.success(),
        "replay helper failed: {}",
        String::from_utf8_lossy(&replayed.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(&marker)
            .expect("read replay effect marker")
            .lines()
            .count(),
        2,
        "recorded effect outcome replays without re-execution"
    );
}

#[tokio::test]
async fn postgres_real_turn_satisfies_cold_process_crash_matrix_when_configured() {
    let Some((_database_lock, storage)) = storage().await else {
        eprintln!(
            "skipping PostgreSQL cold-process real-turn matrix: LASH_POSTGRES_DATABASE_URL is not set"
        );
        return;
    };
    reset(storage.pool()).await;
    let url = database_url().expect("configured PostgreSQL database URL");
    let dir = tempfile::tempdir().expect("PostgreSQL cold-process real-turn tempdir");
    cold_process_turn_parent::assert_real_turn_kill_recovery(
        dir.path(),
        |action, nonce, marker| {
            let mut command = tokio::process::Command::new(lash_conformance::helper_executable(
                "postgres-await-event-helper",
            ));
            command
                .env("LASH_POSTGRES_DATABASE_URL", &url)
                .arg(action)
                .arg(nonce)
                .arg(marker);
            command
        },
    )
    .await;
}
