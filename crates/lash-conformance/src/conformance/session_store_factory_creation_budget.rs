//! FIG-4393: creation measures the head it creates against the commit
//! budget.
//!
//! The commit budget is host policy (ADR 0058). Every commit over a head
//! carries the head's config and checkpoint manifest, so a head whose bare
//! commit exceeds the budget refuses every write, a session command's
//! settlement included. Creation is the one head write outside a runtime
//! commit: it measures the created head's bare commit and refuses a config
//! that no commit fits under, writing nothing.

use super::*;
use pretty_assertions::assert_eq;
use std::future::Future;

/// A byte budget below any head's bare commit: the session config alone
/// exceeds it.
const BELOW_ANY_HEAD_BYTES: usize = 64;

fn byte_budget(bytes: usize) -> crate::CommitBudget {
    crate::CommitBudget::new(
        crate::CommitBudgetLimit::bounded(bytes),
        crate::CommitBudgetLimit::Unbounded,
    )
}

/// Creation measures the head it creates (FIG-4393): a config whose
/// created head's bare commit exceeds the budget is refused with the typed
/// byte-budget error, one byte short of that commit included, and nothing
/// is written; at the commit's own size the session is created. The sizing
/// probe reserves a fixed full-precision timestamp, so repeated admissions
/// measure identical input even when the wall clock's precision changes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_creation_refuses_a_head_no_commit_fits<M, Fut>(make: M)
where
    M: Fn() -> Fut,
    Fut: Future<Output = crate::Backend>,
{
    let backend = make().await;
    let catalog = backend.session_store_factory();
    let fleet = catalog.fleet_format();
    // A root session whose head is the creator's config, as the host's
    // `create` writes it.
    let mut request = session_store_request(
        &SessionId::from("creation-budget"),
        "creation-budget-model",
        crate::SessionRelation::Root,
    );
    request.head = crate::SessionCreationHead::Config;
    let session_id = request.session_id.clone();
    let refusal = |bytes: usize| {
        let catalog = Arc::clone(&catalog);
        let request = request.clone();
        async move {
            match crate::store::admit_created_session(
                catalog.as_ref(),
                &request,
                byte_budget(bytes),
                fleet,
            )
            .await
            {
                Err(crate::StoreError::CommitByteBudgetExceeded {
                    total_bytes,
                    max_bytes,
                    ..
                }) => {
                    assert_eq!(max_bytes, bytes, "the refusal names the host's budget");
                    total_bytes
                }
                other => panic!("a {bytes}-byte budget refuses the creation typed, got {other:?}"),
            }
        }
    };

    let bare_bytes = refusal(BELOW_ANY_HEAD_BYTES).await;
    assert_eq!(
        refusal(bare_bytes - 1).await,
        bare_bytes,
        "one byte short of the created head's bare commit is refused"
    );
    assert!(
        matches!(
            catalog
                .lookup_session(&session_id)
                .await
                .expect("look the refused session up"),
            crate::SessionLookup::Absent
        ),
        "a refused creation writes nothing"
    );

    let created = crate::store::admit_created_session(
        catalog.as_ref(),
        &request,
        byte_budget(bare_bytes),
        fleet,
    )
    .await
    .expect("the created head's bare commit fits its own size");
    assert!(
        matches!(created, crate::SessionAdmission::Created),
        "{created:?}"
    );
    let head = catalog
        .admit_view(&request)
        .await
        .expect("bind the created session")
        .store()
        .load_session_head_meta(&session_id)
        .await
        .expect("read the created head")
        .expect("creation wrote the config head");
    assert_eq!(head.head_revision, 0);
}
