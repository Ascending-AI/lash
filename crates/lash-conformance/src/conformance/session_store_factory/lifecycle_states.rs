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
