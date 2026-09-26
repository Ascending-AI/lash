//! This target's twins on the Restate server double and the storage-only
//! store set (D1 F3/F7, PR-S2).

use std::sync::Arc;

use crate::process_registry::{ProcessDefinitionRegistration, ProcessDefinitionRegistry};
use crate::{SessionId, TriggerOwnerScope};

const SEED: u64 = 0x5_2d20;

fn reference(payload: &str) -> crate::ProcessDefinitionRef {
    crate::ProcessDefinitionRef::unclaimed(
        "test-engine",
        serde_json::json!({"definition": payload}),
    )
}

/// `kernel::process_registry::tests::registration_is_cas_fenced` in its F3
/// form: the registry comes from a storage-only store set, no engine.
#[tokio::test]
async fn a_definition_registration_is_cas_fenced_on_the_store_set() {
    let registry: Arc<dyn ProcessDefinitionRegistry> = crate::support::memory_store_set()
        .await
        .process_definition_registry();
    let scope = TriggerOwnerScope::session(SessionId::from("session-a"));
    let first = registry
        .register_definition(
            "op-1",
            scope.clone(),
            "nightly-scan",
            reference("one"),
            None,
        )
        .await
        .expect("the first registration");
    let ProcessDefinitionRegistration::Admitted(admitted) = &first else {
        panic!("the first registration admits: {first:?}");
    };
    assert_eq!(admitted.revision, 1);
    let error = registry
        .register_definition(
            "op-2",
            scope,
            "nightly-scan",
            reference("one-bis"),
            Some(
                &crate::process_registry::ProcessDefinitionExpectation::observed(
                    7,
                    admitted.fingerprint.clone(),
                ),
            ),
        )
        .await
        .expect_err("a stale expected revision conflicts")
        .to_string();
    assert!(error.contains("conflicts with revision 1"), "{error}");
}

/// The store-only backend reaches the same kind of store port.
#[tokio::test]
async fn the_store_only_backend_serves_its_store_ports() {
    let backend = crate::support::memory_store_backend().await;
    let registration = backend
        .process_definition_registry()
        .register_definition(
            "op-1",
            TriggerOwnerScope::session(SessionId::from("session-b")),
            "hourly",
            reference("two"),
            None,
        )
        .await
        .expect("the store-only backend's registry registers");
    assert!(matches!(
        registration,
        ProcessDefinitionRegistration::Admitted(_)
    ));
}

/// An open handler on this target's double lends a turn-scoped controller
/// and closes cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_handler_lends_its_scope_on_the_double() {
    let double =
        crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let admitted = crate::AdmittedScope::turn(SessionId::from("root"), crate::TurnId::from("t"));
    let handler = double
        .open_handler(admitted.clone())
        .await
        .expect("open the handler");
    assert_eq!(handler.scoped().admitted_scope(), &admitted);
    handler.close().await.expect("close the handler");
}
