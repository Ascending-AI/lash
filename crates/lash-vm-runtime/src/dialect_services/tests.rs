//! K-HOST: service publication and injected reads use ordinary operations.
use super::*;
use lash_core_ids::clock::Clock;
use lash_core_ids::test_clock::TestClock;
use lash_vocabulary::{claims::RegistryKind, providers::ToolProviderKey};

/// K-HOST-001/002: code-only services publish read/read/waited modes, no model
/// manifests, and read through the injected clock/random sources on every call.
#[tokio::test]
async fn ordinary_services_are_code_only_and_use_injected_reads() {
    for dialect in [Dialect::TypeScript, Dialect::Python] {
        let provider = ToolProviderHandle::new(
            RegistryKind::Session,
            dialect.package(),
            ToolProviderKey::new("services").unwrap(),
        );
        let draws = Arc::new(std::sync::Mutex::new(std::collections::VecDeque::from([
            0x0123456789abcdef,
            u64::MAX,
        ])));
        let random = Arc::clone(&draws);
        let services = Services::new(
            dialect,
            provider,
            Duration::from_secs(30),
            Arc::new(move || random.lock().unwrap().pop_front().unwrap()),
        )
        .unwrap();
        let config = OwnerConfigView::default();
        let context = PublicationContext {
            domain: PublicationDomain("law".into()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance(dialect.package().into()),
            root: Segment::new(dialect.package()).unwrap(),
        };
        let [contribution] = services
            .advertise(&config, &context)
            .unwrap()
            .try_into()
            .unwrap();
        let PackageAdvertisement::Enumerated(package) = contribution.advertisement else {
            panic!("enumerated service operations")
        };
        assert_eq!(package.members.len(), 3);
        assert_eq!(contribution.routes.len(), 3);
        assert!(services.tool_manifests(&config).unwrap().is_empty());
        for (member, mode) in [
            ("clock", OperationMode::Read),
            ("random", OperationMode::Read),
            ("sleep", OperationMode::Waited),
        ] {
            let MemberKind::Operation(offer) =
                &package.members[&Segment::new(member).unwrap()].kind
            else {
                panic!("ordinary operation")
            };
            assert_eq!(offer.mode, mode);
            let definition = services
                .definitions
                .get(&format!("{}.{member}", dialect.package()))
                .unwrap();
            assert_eq!(
                lash_vocabulary::JsonSchema::admit(
                    definition.contract.output_schema.canonical().clone()
                )
                .unwrap()
                .result_type(),
                &offer.requirement.signature.result,
            );
        }
        let clock = TestClock::new(1234);
        let cancel = tokio_util::sync::CancellationToken::new();
        for (member, expected) in [
            ("clock", "1234000000"),
            ("random", "0123456789abcdef"),
            ("random", "ffffffffffffffff"),
        ] {
            let manifest =
                &services.definitions[&format!("{}.{member}", dialect.package())].manifest;
            let value = services
                .read(ReadCall {
                    history: None,
                    clock: &clock,
                    manifest,
                    args: &serde_json::json!({}),
                    config: &config,
                    cancel: &cancel,
                    deadline: clock.now() + Duration::from_secs(1),
                })
                .await
                .unwrap();
            assert_eq!(value, serde_json::json!(expected));
        }
        assert!(draws.lock().unwrap().is_empty());
    }
}

/// K-EFF-010: the ordinary sleep runner records the admitted absolute
/// deadline, including saturation, in the durable pending result.
#[tokio::test]
async fn sleep_records_the_admitted_absolute_deadline_for_redrive() {
    use lash_core_execution::{PendingResolver, testing::ToolCallFixture};
    use lash_durable::DurableInstant;
    use lash_execution_contracts::waits::PinnedKey;

    for dialect in [Dialect::TypeScript, Dialect::Python] {
        let services = Services::new(
            dialect,
            ToolProviderHandle::new(
                RegistryKind::Session,
                dialect.package(),
                ToolProviderKey::new("timer").unwrap(),
            ),
            Duration::from_secs(30),
            Arc::new(|| 0),
        )
        .unwrap();
        let definition = &services.definitions[&format!("{}.sleep", dialect.package())];
        let fixture = ToolCallFixture::mock();
        for (admitted, milliseconds, expected) in [(1000, 250, 1250), (i64::MAX - 1, 250, i64::MAX)]
        {
            let context = fixture.admitted_attempt(
                "sleep",
                PinnedKey::of(&lash_durable::domain::WaitId([3; 16])),
                DurableInstant(admitted),
            );
            let arguments = serde_json::json!(milliseconds);
            let ToolAttemptOutcome::Pending(pending) = services
                .execute(ToolCall::new(
                    &definition.manifest,
                    &definition.contract,
                    &arguments,
                    &context,
                ))
                .await
            else {
                panic!("ordinary durable timer")
            };
            let encoded = serde_json::to_vec(&pending).unwrap();
            let restored: PendingCompletion = serde_json::from_slice(&encoded).unwrap();
            assert_eq!(
                restored.resolved_by,
                Some(PendingResolver::Timer {
                    deadline: DurableInstant(expected),
                    result: serde_json::Value::Null
                })
            );
            assert_eq!(restored, pending);
        }
    }
}
