use super::*;
use pretty_assertions::assert_eq;

/// A session whose close has begun lists as closing, never as live: the
/// catalog reads the same `closing_intent` the close wrote, so the listing
/// and the deletion answer cannot disagree. Its relation is still its
/// recorded one, and its tombstone keeps only the coarse kind and parent.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture: each result is established by the setup above"
)]
pub async fn a_closing_session_lists_as_closing_never_as_live(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
) {
    let parent = SessionId::from("catalog-closing-parent");
    let id = SessionId::from("catalog-closing");
    let relation = crate::SessionRelation::Child {
        parent_session_id: parent.clone(),
        caused_by: None,
    };
    let request = session_store_request(&id, "catalog-closing-model", relation.clone());
    factory
        .admit_view(&request)
        .await
        .expect("create the session");
    let entry = || async {
        factory
            .list_sessions(&crate::SessionListFilter::default())
            .await
            .expect("list the catalog")
            .into_iter()
            .find(|view| view.session_id == id)
            .expect("the session is listed")
            .entry
    };
    assert_eq!(
        entry().await,
        crate::SessionEntry::Live {
            relation: relation.clone()
        }
    );

    factory
        .begin_session_close(&id, 7)
        .await
        .expect("begin the close")
        .expect("the session exists");
    assert_eq!(
        entry().await,
        crate::SessionEntry::Closing {
            relation: relation.clone()
        },
        "a session whose close has begun never lists as live"
    );
    let undeleted = factory
        .list_sessions(&crate::SessionListFilter {
            deleted: Some(false),
            ..crate::SessionListFilter::default()
        })
        .await
        .expect("list the undeleted sessions");
    assert!(
        undeleted
            .iter()
            .any(|view| view.session_id == id && !view.is_deleted()),
        "a closing session is not yet deleted: {undeleted:?}"
    );

    factory
        .delete_session(&id)
        .await
        .expect("delete the closed session");
    assert_eq!(
        entry().await,
        crate::SessionEntry::Deleted {
            kind: crate::SessionRelationKind::Child,
            parent: Some(parent),
        }
    );
}

/// A raise that stored no start marker is a control verb's: no execution
/// sealed it, so a seal presenting its admission is never answered `Sealed`.
/// Before FIG-4661 the seal decision read the missing marker as a seal that
/// predated markers and answered the stored fence to a fresh execution.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture: each result is established by the setup above"
)]
pub async fn a_control_raise_answers_no_seal_as_sealed(
    factory: Arc<dyn crate::store::ConformanceDeployment>,
) {
    let id = SessionId::from("control-raise-seal");
    let request = session_store_request(
        &id,
        "control-raise-seal-model",
        crate::SessionRelation::Root,
    );
    let store = factory
        .admit_view(&request)
        .await
        .expect("create the session");
    assert_eq!(
        store.drive_epoch().await.expect("read the unraised epoch"),
        crate::store::StoredDriveEpoch::unraised()
    );

    let intent = factory
        .begin_session_close(&id, 7)
        .await
        .expect("begin the close")
        .expect("the session exists");
    let raised_by = crate::store::close_admission(intent.id);
    let stored = store.drive_epoch().await.expect("read the raised epoch");
    assert_eq!(
        stored,
        crate::store::StoredDriveEpoch {
            epoch: 1,
            last_raise: Some(crate::store::DriveRaise::Control {
                admission: raised_by.clone(),
            }),
            closing: Some(intent.id),
            control_pending: false,
        }
    );

    for observed in [0, stored.epoch] {
        let seal = store
            .seal_drive_epoch(
                &raised_by,
                observed,
                &crate::store::RootStartNonce::new("a-fresh-execution"),
                None,
            )
            .await
            .expect("decide the seal");
        assert_eq!(
            seal,
            crate::store::DriveEpochSeal::Superseded {
                epoch: stored.epoch
            },
            "observed epoch {observed}: no execution sealed a control raise"
        );
    }
    assert_eq!(
        store.drive_epoch().await.expect("re-read the epoch"),
        stored,
        "the refused seal wrote nothing"
    );
}
