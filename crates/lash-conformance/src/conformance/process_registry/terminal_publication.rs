//! A terminal process owes its engine waiters its terminal (ADR 0109 §3,
//! `ProcessTerminal`): the terminal transaction arms the obligation on the
//! process row, once, and a publishing engine settles it.

use super::*;
use lash_core::store::ObligationState;
use pretty_assertions::assert_eq;

async fn publication(
    registry: &Arc<dyn ProcessRegistry>,
    process_id: &ProcessId,
) -> Option<crate::ProcessTerminalPublication> {
    registry
        .terminal_publication(process_id)
        .await
        .unwrap_or_else(|error| panic!("read {process_id}'s terminal publication: {error}"))
}

fn registration() -> ProcessRegistration {
    ProcessRegistration::new(
        ProcessInput::External {
            metadata: serde_json::Value::Null,
        },
        RecoveryContract::Rerunnable,
        ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub(super) async fn a_terminal_write_arms_its_publication_once(registry: Arc<dyn ProcessRegistry>) {
    // The unleased completion a workflow-key substrate writes.
    let keyed = registry
        .register_process(registration())
        .await
        .expect("register the keyed process")
        .id;
    assert_eq!(
        publication(&registry, &keyed).await,
        None,
        "a live process owes no terminal publication"
    );
    registry
        .complete_process(
            &keyed,
            settled_success(serde_json::json!({ "ended": "keyed" })),
            crate::ProcessCompletionAuthority::WorkflowKey {
                workflow_key: keyed.to_string(),
            },
        )
        .await
        .expect("complete the keyed process");
    let armed = publication(&registry, &keyed)
        .await
        .expect("the terminal transaction arms the publication");
    assert_eq!(armed.state, ObligationState::Due);

    // A replayed completion writes nothing and arms nothing again.
    let replay = registry
        .complete_process(
            &keyed,
            settled_success(serde_json::json!({ "ended": "keyed" })),
            crate::ProcessCompletionAuthority::WorkflowKey {
                workflow_key: keyed.to_string(),
            },
        )
        .await
        .expect("replay the completion");
    assert!(!matches!(
        replay,
        crate::ProcessCompletionOutcome::Committed(_)
    ));
    assert_eq!(
        publication(&registry, &keyed).await,
        Some(armed.clone()),
        "a replayed terminal keeps the one obligation its first commit armed"
    );

    // The leased completion a native worker writes arms the same obligation.
    let leased = registry
        .register_process(registration())
        .await
        .expect("register the leased process")
        .id;
    let lease = registry
        .claim_process_lease(
            &leased,
            &crate::LeaseOwnerIdentity::opaque("publication-owner", "publication-owner:i"),
            60_000,
        )
        .await
        .expect("claim the leased process")
        .acquired()
        .expect("the lease is acquired");
    registry
        .complete_process_with_lease(
            &lease,
            settled_success(serde_json::json!({ "ended": "leased" })),
        )
        .await
        .expect("complete the leased process");
    let leased_publication = publication(&registry, &leased)
        .await
        .expect("the leased terminal arms its publication");
    assert_eq!(leased_publication.state, ObligationState::Due);
    assert_ne!(
        leased_publication.id, armed.id,
        "each terminal owes its own obligation"
    );

    // The engine that published settles it, once.
    assert!(
        registry
            .settle_terminal_publication(&keyed)
            .await
            .expect("settle the keyed publication"),
        "an owed publication settles"
    );
    assert_eq!(
        publication(&registry, &keyed).await,
        Some(crate::ProcessTerminalPublication {
            id: armed.id,
            state: ObligationState::Delivered,
        })
    );
    assert!(
        !registry
            .settle_terminal_publication(&keyed)
            .await
            .expect("settle the keyed publication again"),
        "a delivered publication settles nothing"
    );
    assert_eq!(
        publication(&registry, &leased)
            .await
            .map(|publication| publication.state),
        Some(ObligationState::Due),
        "settling one process's publication leaves another's owed"
    );

    // A live process has nothing to settle.
    let live = registry
        .register_process(registration())
        .await
        .expect("register the live process")
        .id;
    assert!(
        !registry
            .settle_terminal_publication(&live)
            .await
            .expect("settle a live process"),
        "a live process owes no publication"
    );
    assert_eq!(publication(&registry, &live).await, None);
}
