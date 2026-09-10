use std::sync::Arc;

use lash_core::{
    AwaitEventKey, AwaitEventResolver, EffectHost, Resolution, ResolveOutcome, SessionStoreFactory,
};
use lash_sqlite_store::SqliteEffectHost;
use tokio::io::{AsyncBufReadExt as _, BufReader};
use tokio::process::Command;

#[tokio::test]
async fn sqlite_effect_host_satisfies_cold_process_await_event_conformance() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cold-process-await-event.db");
    for identity in ["tool_completion", "turn_cancel_gate"] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new(lash_conformance::helper_executable(
            "sqlite-await-event-helper",
        ))
        .arg(&path)
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
            SqliteEffectHost::open(&path)
                .await
                .expect("cold-process resolver"),
        );
        let terminal = if identity == "turn_cancel_gate" {
            let address = lash_core::runtime::TurnAddress::new(
                format!("cold-process-{nonce}-session"),
                format!("cold-process-{nonce}-turn"),
            );
            let store_factory: Arc<dyn SessionStoreFactory> =
                Arc::new(lash_core::runtime::InMemorySessionStoreFactory::new());
            store_factory
                .create_store(&lash_core::SessionStoreCreateRequest {
                    pending_observer_intents: Vec::new(),
                    session_id: address.session_id.clone(),
                    relation: lash_core::SessionRelation::Root,
                    policy: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
                })
                .await
                .expect("create cold-process cancellation session");
            let receipt = lash_core::runtime::TurnWorkDriver::for_catalog(
                Arc::clone(&resolver) as Arc<dyn EffectHost>,
                store_factory,
            )
            .request_cancel(lash_core::runtime::TurnCancelRequest::new(
                address,
                format!("cold-process-{nonce}-cancel"),
                None,
            ))
            .await
            .expect("request cancellation through a successor owner");
            assert!(matches!(
                receipt.outcome,
                lash_core::runtime::TurnCancelOutcome::Requested(_)
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

        let observer = SqliteEffectHost::open(&path)
            .await
            .expect("cold-process observer");
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
