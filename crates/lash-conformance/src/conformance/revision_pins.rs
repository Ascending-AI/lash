//! FIG-4731: a state is named `(session, head revision)`, every head
//! publication is a retained revision until the host collects, and a pin is a
//! name (an input, a turn or a revision) that only host collection reads.
//!
//! A fork "is" a revision when the fork's head reads back the window,
//! checkpoint, frame and model that revision published. The laws over runs
//! the session actor commits are owed to its turn laws (FIG-5196).

use super::session_store_factory::session_store_request;
use super::*;
use lash_core::store::ConformanceDeployment;
use lash_sansio::SessionId;
use pretty_assertions::assert_eq;

/// One session a law executes turns on.
struct PinLaw {
    factory: Arc<dyn ConformanceDeployment>,
    request: crate::SessionStoreCreateRequest,
    view: crate::store::SessionStore,
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
impl PinLaw {
    async fn new(factory: &Arc<dyn ConformanceDeployment>, session: &str) -> Self {
        let request = session_store_request(
            &SessionId::fixture(session),
            "revision-pin-model",
            crate::SessionRelation::Root,
        );
        let view = factory
            .admit_view(&request)
            .await
            .expect("create the law's session");
        Self {
            factory: Arc::clone(factory),
            request,
            view,
        }
    }

    fn id(&self) -> &SessionId {
        &self.request.session_id
    }

    fn store(&self) -> &Arc<dyn crate::RuntimeStore> {
        self.view.store()
    }

    /// What `session`'s head reads back: the resume closure a fork must
    /// reproduce.
    async fn head_state(&self, session: &SessionId) -> serde_json::Value {
        let read = self
            .store()
            .load_session_window(session, crate::store::WindowSelector::Current)
            .await
            .expect("read the head")
            .expect("the session has a head");
        serde_json::json!({
            "window": read.window,
            "checkpoint_ref": read.checkpoint_ref,
            "checkpoint": read.checkpoint,
            "current_frame_node_id": read.current_frame_node_id,
            "model": read.config.model,
        })
    }

    fn branch(&self, name: &str) -> SessionId {
        SessionId::fixture(format!("{}-{name}", self.id()))
    }

    /// Fork `target` into the branch `name`, as the facade does: resolve the
    /// target, then fork the revision it names.
    async fn fork(
        &self,
        target: &crate::Target,
        name: &str,
    ) -> Result<SessionId, crate::StoreError> {
        let revision = self.factory.resolve_target(self.id(), target).await?;
        let session_id = self.branch(name);
        self.factory
            .fork_session(&crate::ForkSessionRequest {
                session_id: session_id.clone(),
                source_session_id: self.id().clone(),
                head_revision: revision.head_revision,
                relation: crate::SessionRelation::Fork {
                    source_session_id: self.id().clone(),
                    source_node_id: revision.leaf_node_id.clone(),
                },
                pending_observer_intents: Vec::new(),
                config: revision.fork_config(),
            })
            .await?;
        Ok(session_id)
    }
}

/// D4: a session that never ran a turn forks at its creation revision. The
/// fork copies the recorded configuration and records its lineage with no
/// source node.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_session_that_never_ran_a_turn_forks_at_its_creation_revision(
    factory: Arc<dyn ConformanceDeployment>,
) {
    let law = PinLaw::new(&factory, "fork-empty").await;
    let created = law
        .factory
        .resolve_target(law.id(), &crate::Target::Revision(0))
        .await
        .expect("the creation revision is retained");
    assert!(created.head);
    assert_eq!(created.leaf_node_id, None, "nothing has been appended");
    assert_eq!(created.config.model, law.request.config.model);

    let fork = law
        .fork(&crate::Target::Revision(0), "branch")
        .await
        .expect("an empty session forks at its creation revision");
    assert_eq!(
        law.head_state(&fork).await,
        law.head_state(law.id()).await,
        "the fork copies the empty session's recorded head"
    );
    assert_eq!(
        law.store()
            .load_session_meta(&fork)
            .await
            .expect("read the fork's metadata")
            .expect("the fork has metadata")
            .relation,
        crate::SessionRelation::Fork {
            source_session_id: law.id().clone(),
            source_node_id: None,
        },
        "the fork records its lineage; there is no source node to name"
    );
    // The fork is an ordinary session: its own creation revision is its head.
    assert_eq!(
        factory
            .revisions(&fork)
            .await
            .expect("list the fork's revisions")
            .into_iter()
            .map(|revision| (revision.head_revision, revision.leaf_node_id, revision.head))
            .collect::<Vec<_>>(),
        vec![(0, None, true)]
    );
}
