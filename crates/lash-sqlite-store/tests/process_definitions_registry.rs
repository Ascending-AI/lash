use std::sync::Arc;

use lash_core::process_registry::{ProcessDefinitionExpectation, ProcessDefinitionRegistry};
use lash_core::{ProcessDefinitionRef, SessionId, TriggerOwnerScope};

fn reference(payload: &str) -> ProcessDefinitionRef {
    ProcessDefinitionRef::unclaimed("test-engine", serde_json::json!({"definition": payload}))
}

fn clock() -> Arc<dyn lash_core::Clock> {
    Arc::new(lash_core::facade_support::SystemClock)
}

#[test]
fn sqlite_registration_is_cas_fenced_and_durable() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(async move {
        let registry = Arc::new(
            lash_sqlite_store::SqliteProcessDefinitionRegistry::memory_with_clock(clock())
                .await
                .unwrap(),
        );
        let scope = TriggerOwnerScope::session(SessionId::from("session-a"));
        let admitted = match registry
            .register_definition(
                "op-1",
                scope.clone(),
                "nightly-scan",
                reference("one"),
                None,
            )
            .await
            .unwrap()
        {
            lash_core::ProcessDefinitionRegistration::Admitted(record) => *record,
            other => panic!("first registration admits: {other:?}"),
        };
        assert_eq!(admitted.revision, 1);

        // A stale expected revision conflicts, typed.
        let error = registry
            .register_definition(
                "op-2",
                scope.clone(),
                "nightly-scan",
                reference("one-bis"),
                Some(&ProcessDefinitionExpectation::observed(
                    7,
                    admitted.fingerprint.clone(),
                )),
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("conflicts with revision 1"),
            "conflict must be typed: {error}"
        );

        // A take-over without the caller's revision CAS refuses.
        registry
            .register_definition(
                "op-3",
                scope.clone(),
                "nightly-scan",
                reference("two"),
                None,
            )
            .await
            .unwrap_err();

        // With the caller's observed state the take-over advances the
        // revision and re-pins, and a fresh read observes it.
        let takeover = registry
            .register_definition(
                "op-2",
                scope.clone(),
                "nightly-scan",
                reference("two"),
                Some(&ProcessDefinitionExpectation::observed(
                    1,
                    admitted.fingerprint.clone(),
                )),
            )
            .await
            .unwrap();
        assert_eq!(takeover.record().revision, 2);
        assert_eq!(
            takeover.record().definition.fingerprint(),
            reference("two").fingerprint()
        );
        let listed = registry.list_definitions(&scope).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].revision, 2);
        assert_eq!(listed[0].name, "nightly-scan");
        assert_eq!(
            listed[0].definition.fingerprint(),
            reference("two").fingerprint()
        );

        // A different owner scope is a distinct namespace.
        registry
            .register_definition(
                "op-3",
                TriggerOwnerScope::session(SessionId::from("session-b")),
                "nightly-scan",
                reference("other-owner"),
                None,
            )
            .await
            .unwrap();
        let listed = registry
            .list_definitions(&TriggerOwnerScope::session(SessionId::from("session-a")))
            .await
            .unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].definition.fingerprint(),
            reference("two").fingerprint()
        );
    });
}
