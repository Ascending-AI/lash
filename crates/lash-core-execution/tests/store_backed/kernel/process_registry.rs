mod tests {
    use std::sync::Arc;

    use crate::process_registry::*;
    use crate::{SessionId, TriggerOwnerScope};

    fn reference(payload: &str) -> crate::ProcessDefinitionRef {
        crate::ProcessDefinitionRef::unclaimed(
            "test-engine",
            serde_json::json!({"definition": payload}),
        )
    }

    fn scope() -> TriggerOwnerScope {
        TriggerOwnerScope::session(SessionId::from("session-a"))
    }

    async fn reg() -> Arc<dyn ProcessDefinitionRegistry> {
        crate::support::memory_store_set()
            .await
            .process_definition_registry()
    }

    #[tokio::test]
    async fn registration_is_cas_fenced() {
        let reg = reg().await;
        {
            let first = reg
                .register_definition("op-1", scope(), "nightly-scan", reference("one"), None)
                .await
                .unwrap();
            let admitted = match &first {
                ProcessDefinitionRegistration::Admitted(record) => record,
                other => panic!("first registration admits: {other:?}"),
            };
            assert_eq!(admitted.revision, 1);
            assert_eq!(
                admitted.definition.fingerprint(),
                reference("one").fingerprint()
            );

            // A stale expected revision conflicts, typed.
            let error = reg
                .register_definition(
                    "op-2",
                    scope(),
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
            let take_over = reg
                .register_definition("op-2", scope(), "nightly-scan", reference("two"), None)
                .await;
            assert!(
                take_over
                    .unwrap_err()
                    .to_string()
                    .contains("conflicts with"),
                "silent take-over is refused"
            );

            // With the caller's observed revision the take-over advances the
            // revision and re-pins.
            let takeover = reg
                .register_definition(
                    "op-2",
                    scope(),
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
        }
    }

    /// Red-side dual of the pinned-at-registration law: the registry answer a
    /// trigger would get by re-resolving the name after a take-over follows
    /// the new definition, so the ref a trigger pins must be captured at
    /// registration, not at delivery.
    #[tokio::test]
    async fn a_name_resolves_once_and_the_record_pins_the_reference() {
        let reg = reg().await;
        {
            reg.register_definition("op-1", scope(), "nightly-scan", reference("one"), None)
                .await
                .unwrap();
            let pinned_record = resolve_named_definition(
                reg.as_ref(),
                &SessionId::from("session-a"),
                "nightly-scan",
            )
            .await
            .unwrap();
            let pinned_definition = pinned_record
                .as_ref()
                .map(|record| record.definition.clone())
                .unwrap();
            // The caller takes the name over to a different definition.
            reg.register_definition(
                "op-2",
                scope(),
                "nightly-scan",
                reference("two"),
                Some(&ProcessDefinitionExpectation::observed(
                    1,
                    pinned_definition.fingerprint(),
                )),
            )
            .await
            .unwrap();
            // A delivery that re-resolved the name today would fire `two`:
            // exactly what ADR 0011 forbids. The pinned record still carries
            // `one`.
            let re_resolved = resolve_named_definition(
                reg.as_ref(),
                &SessionId::from("session-a"),
                "nightly-scan",
            )
            .await
            .unwrap()
            .unwrap();
            assert_ne!(
                pinned_definition.fingerprint(),
                re_resolved.definition.fingerprint(),
                "red-side: re-resolution follows the take-over"
            );
            assert_eq!(
                pinned_definition.fingerprint(),
                reference("one").fingerprint(),
                "the pinned reference captured at registration time is unchanged"
            );
        }
    }

    #[tokio::test]
    async fn a_fenced_or_unregistered_name_refuses_resolution() {
        let reg = reg().await;
        {
            let resolved = resolve_named_definition(
                reg.as_ref(),
                &SessionId::from("session-a"),
                "unregistered-name",
            )
            .await
            .unwrap();
            assert!(resolved.is_none(), "an unregistered name resolves nothing");
            let error = validate_process_definition_name("lash.internal/x").unwrap_err();
            assert!(error.to_string().contains("reserved prefix"), "{error}");
        }
    }

    /// A pinned reference to a definition whose artifact the engine never
    /// stored refuses the start with the engine's typed reason, not a panic
    /// or a silent fallback. This is what a durable record full of
    /// content-addressed refs means: the engine resolves the ref at
    /// start time and every miss is a typed refusal (FIG-2995).
    #[tokio::test]
    async fn a_registry_reference_to_a_missing_artifact_refuses_with_a_typed_reason() {
        let reg = reg().await;
        {
            reg.register_definition("op-1", scope(), "nightly-scan", reference("one"), None)
                .await
                .unwrap();
            let record = resolve_named_definition(
                reg.as_ref(),
                &SessionId::from("session-a"),
                "nightly-scan",
            )
            .await
            .unwrap()
            .unwrap();
            // No engine of this kind is installed on this host: the pinned
            // reference cannot resolve.
            let registry = crate::ProcessEngineRegistry::default();
            let refusal = registry.resolve(&record.definition).await.unwrap_err();
            assert!(
                matches!(
                    refusal,
                    crate::ProcessDefinitionRefusal::UnknownEngine { .. }
                ),
                "missing artifact is a typed refusal: {refusal:?}"
            );
        }
    }
}
