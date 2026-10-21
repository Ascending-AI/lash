//! FIG-6611: a session holds an attachment for a named reason (its committed
//! history, a pending input or a queued batch), and the session's edge lasts
//! exactly as long as one of those holds does.
//!
//! A live session's committed history never stops naming what it named: its
//! head only extends, so history holds last as long as the session graph.
//! An input's hold ends with its payload: vacuum, which deletes a withdrawn
//! input, ends it. The session edge is then dropped at the revision-release
//! boundary, a commit that releases revisions or the host's collection, and
//! ordinary fenced reclamation deletes the bytes.

use super::session_delete_blob_reclaim::SessionDeleteBlobHandles;
use super::session_store_factory::session_store_request;
use super::*;
use lash_vocabulary::identity::SessionId;

const MODEL: &str = "session-attachment-hold-model";

/// One session a law runs on, and the port its attachments go through.
struct HoldLaw {
    handles: SessionDeleteBlobHandles,
    session_id: SessionId,
    view: crate::store::SessionStore,
    facade: crate::RuntimeAttachmentStore,
}

fn message(id: &str, references: &[&crate::AttachmentRef]) -> crate::Message {
    crate::Message {
        id: id.into(),
        role: crate::MessageRole::User,
        origin: None,
        parts: Arc::new(
            references
                .iter()
                .enumerate()
                .map(|(index, reference)| {
                    crate::Part::attachment_part(
                        format!("{id}-part-{index}"),
                        String::new(),
                        Some(lash_protocol_types::PartAttachment {
                            reference: (*reference).clone(),
                        }),
                    )
                })
                .collect(),
        ),
        reply_marker: None,
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
impl HoldLaw {
    async fn new(
        handles: SessionDeleteBlobHandles,
        name: &str,
        retention: crate::Retention,
    ) -> Self {
        let request = session_store_request(
            &SessionId::fixture(name.to_owned()),
            MODEL,
            crate::SessionRelation::Root,
        );
        let view = handles
            .factory
            .admit_view(&request)
            .await
            .expect("create the law's session");
        handles
            .factory
            .set_retention(&request.session_id, retention)
            .await
            .expect("record the session's retention");
        let facade = crate::RuntimeAttachmentStore::new(
            Arc::clone(&handles.attachments),
            Arc::clone(view.store()) as Arc<dyn crate::AttachmentReferrers>,
            crate::RuntimeOwner::Session(request.session_id.clone()),
            crate::AttachmentPolicy::standard(),
        );
        Self {
            handles,
            session_id: request.session_id,
            view,
            facade,
        }
    }

    fn store(&self) -> &Arc<dyn crate::RuntimeStore> {
        self.view.store()
    }

    async fn reopen(&mut self) {
        self.reopen_on((self.handles.reopen)()).await;
    }

    async fn reopen_on(&mut self, factory: Arc<dyn crate::DeploymentStore>) {
        self.handles.factory = factory;
        let request = session_store_request(&self.session_id, MODEL, crate::SessionRelation::Root);
        self.view = self
            .handles
            .factory
            .live_view_for(&request)
            .await
            .expect("reopen the catalog")
            .expect("the session remains live");
        self.facade = crate::RuntimeAttachmentStore::new(
            Arc::clone(&self.handles.attachments),
            Arc::clone(self.view.store()) as Arc<dyn crate::AttachmentReferrers>,
            crate::RuntimeOwner::Session(self.session_id.clone()),
            crate::AttachmentPolicy::standard(),
        );
    }

    async fn put(&self, bytes: &[u8]) -> crate::AttachmentRef {
        self.facade
            .put(
                bytes.to_vec(),
                crate::AttachmentCreateMeta::new(
                    crate::MediaType::parse("image/png").expect("png media type"),
                    None,
                    None,
                ),
            )
            .await
            .expect("put the attachment")
    }

    /// End the upload staging of `references`, so only the session holds
    /// them.
    async fn end_staging(&self, references: &[&crate::AttachmentRef]) {
        for reference in references {
            for referrer in self
                .store()
                .attachment_referrers(&reference.id)
                .await
                .expect("read the referrers")
            {
                if matches!(referrer, crate::ArtifactReferrer::Upload(_)) {
                    self.store()
                        .end_attachment_referrer(&referrer)
                        .await
                        .expect("end the upload staging");
                }
            }
        }
    }

    /// Publish `edit` over the current head, naming `named`.
    async fn commit(
        &self,
        edit: impl FnOnce(&mut crate::RuntimeSessionState),
        named: &[&crate::AttachmentRef],
    ) {
        let mut state = match super::helpers::load_window_state(self.store(), &self.session_id)
            .await
            .expect("hydrate the current head")
        {
            Some(state) => state,
            None => crate::RuntimeSessionState {
                session_id: self.session_id.clone(),
                ..crate::RuntimeSessionState::fixture(
                    session_store_request(&self.session_id, MODEL, crate::SessionRelation::Root)
                        .config
                        .session_policy(),
                )
            },
        };
        state.ensure_agent_frame_initialized();
        edit(&mut state);
        let commit = crate::RuntimeCommit::persisted_state_for_test(&state)
            .with_committed_attachments(named.iter().map(|reference| reference.id.clone()));
        self.store()
            .commit_runtime_state(commit)
            .await
            .expect("publish the edit");
    }

    async fn enqueue(
        &self,
        source_key: &str,
        references: &[&crate::AttachmentRef],
    ) -> crate::PendingTurnInput {
        let input = references
            .iter()
            .fold(crate::TurnInput::text(source_key), |input, reference| {
                input.with_attachment((*reference).clone())
            });
        self.view
            .enqueue_pending_turn_input(
                crate::PendingTurnInputDraft::new(
                    &self.session_id,
                    crate::TurnInputIngress::NextTurn,
                    input,
                )
                .with_source_key(source_key),
            )
            .await
            .expect("enqueue the input")
    }

    async fn session_holds(&self, reference: &crate::AttachmentRef) -> bool {
        self.store()
            .attachment_referrers(&reference.id)
            .await
            .expect("read the referrers")
            .contains(&crate::ArtifactReferrer::Session(self.session_id.clone()))
    }

    async fn reclaim(&self) {
        crate::reclaim_unreferenced_attachments(
            self.handles.factory.as_ref(),
            self.handles.attachments.as_ref(),
            crate::AttachmentReclamationPolicy::new(
                0,
                crate::EmptyRootSetPolicy::AuthorizeDeleteAll,
            ),
        )
        .await
        .expect("reclaim unreferenced attachments");
    }

    async fn bytes_absent(&self, reference: &crate::AttachmentRef) -> bool {
        matches!(
            self.handles
                .attachments
                .get(&reference.id, 32 * 1024 * 1024)
                .await,
            Err(crate::AttachmentStoreError::NotFound(_))
        )
    }
}

/// `SESSION-ATTACHMENT-HOLDS-FOLLOW-THEIR-HOLDERS`: a withdrawn input's
/// attachment stays held until vacuum deletes the input. Then the session
/// lets it go at the next revision release, a commit under `HeadOnly` or the
/// host's collection under `UntilGc`, and its bytes are reclaimed. The same
/// session's history attachment, an attachment both the withdrawn input and
/// history name, and a pending input's attachment all stay held and
/// readable. Forgetting reads the same relation: it refuses what a holder
/// names and lets go what none does, though the session still has live
/// nodes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn session_attachment_holds_follow_their_holders<F>(backend: &str, make: F)
where
    F: Fn() -> SessionDeleteBlobHandles,
{
    bounded_attachment_release(make()).await;
    for (label, retention) in [
        ("head-only", crate::Retention::HeadOnly),
        ("until-gc", crate::Retention::UntilGc),
    ] {
        let law = HoldLaw::new(make(), &format!("attachment-holds-{label}"), retention).await;
        let history = law.put(format!("history-{label}").as_bytes()).await;
        let shared = law.put(format!("shared-{label}").as_bytes()).await;
        let forgotten = law.put(format!("forgotten-{label}").as_bytes()).await;
        let released = law.put(format!("released-{label}").as_bytes()).await;
        let pending = law.put(format!("pending-{label}").as_bytes()).await;
        law.commit(
            |state| {
                state
                    .session_graph
                    .append_message(message("history", &[&history, &shared]));
            },
            &[&history, &shared],
        )
        .await;
        let withdrawn = law
            .enqueue("withdrawn", &[&shared, &forgotten, &released])
            .await;
        law.enqueue("pending", &[&pending]).await;
        law.end_staging(&[&history, &shared, &forgotten, &released, &pending])
            .await;
        law.view
            .cancel_pending_turn_input(withdrawn.input_id.as_str())
            .await
            .expect("withdraw the input");
        for reference in [&history, &shared, &forgotten, &pending] {
            law.facade
                .delete(&reference.id)
                .await
                .expect("forget through the session");
            assert!(
                law.session_holds(reference).await,
                "{backend}/{label}: a holder still names `{}`",
                reference.id
            );
        }
        let report = law.view.vacuum().await.expect("vacuum the session");
        assert_eq!(
            report.removed_pending_turn_input_tombstone_count, 1,
            "{backend}/{label}"
        );
        law.facade
            .delete(&forgotten.id)
            .await
            .expect("forget through the session");
        assert!(
            !law.session_holds(&forgotten).await,
            "{backend}/{label}: no holder names the forgotten attachment"
        );
        if retention == crate::Retention::UntilGc {
            law.handles
                .factory
                .gc_unreachable()
                .await
                .expect("the host collection completes");
        } else {
            law.commit(
                |state| {
                    state.session_graph.append_message(message("next", &[]));
                },
                &[],
            )
            .await;
        }
        assert!(
            !law.session_holds(&released).await,
            "{backend}/{label}: the release dropped the vacuumed input's hold"
        );
        for reference in [&history, &shared, &pending] {
            assert!(
                law.session_holds(reference).await,
                "{backend}/{label}: `{}` keeps its holder",
                reference.id
            );
        }
        law.reclaim().await;
        assert!(law.bytes_absent(&forgotten).await, "{backend}/{label}");
        assert!(law.bytes_absent(&released).await, "{backend}/{label}");
        for reference in [&history, &shared, &pending] {
            law.facade.read(reference).await.unwrap_or_else(|error| {
                panic!(
                    "{backend}/{label}: `{}` stays readable: {error}",
                    reference.id
                )
            });
        }
    }
}

/// FIG-6611: a published continuation survives reopening and makes progress
/// without another owner commit, both within one session and across sessions.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture checks every operation"
)]
async fn bounded_attachment_release(handles: SessionDeleteBlobHandles) {
    let mut law = HoldLaw::new(handles, "attachment-page", crate::Retention::HeadOnly).await;
    let history = law.put(b"page-history").await;
    let mut released = Vec::new();
    for index in 0..129 {
        released.push(law.put(format!("page-{index}").as_bytes()).await);
    }
    law.commit(
        |state| {
            state
                .session_graph
                .append_message(message("page-history", &[&history]));
        },
        &[&history],
    )
    .await;
    let references: Vec<_> = released.iter().collect();
    let withdrawn = law.enqueue("page-withdrawn", &references).await;
    law.end_staging(&references).await;
    law.end_staging(&[&history]).await;
    law.view
        .cancel_pending_turn_input(withdrawn.input_id.as_str())
        .await
        .expect("withdraw");
    let vacuum = law
        .view
        .vacuum()
        .await
        .expect("queue the ended holder's attachments");
    assert_eq!(vacuum.removed_pending_turn_input_tombstone_count, 1);
    // One HeadOnly owner commit runs exactly one release page. The history
    // neighbor is live but is not part of this release queue.
    law.commit(
        |state| {
            state
                .session_graph
                .append_message(message("page-next", &[]));
        },
        &[],
    )
    .await;
    assert_eq!(
        held_count(&law, &released).await,
        1,
        "one page releases exactly 128 edges"
    );
    law.reopen().await;
    assert_eq!(
        held_count(&law, &released).await,
        1,
        "the outstanding edge is durable"
    );
    law.handles
        .factory
        .gc_unreachable()
        .await
        .expect("continue the queued page");
    assert_eq!(held_count(&law, &released).await, 0);
    assert!(
        law.session_holds(&history).await,
        "history keeps its exact share"
    );
    law.reclaim().await;
    for reference in &released {
        assert!(law.bytes_absent(reference).await);
    }
    law.facade
        .read(&history)
        .await
        .expect("history bytes survive");

    // Queue one otherwise unheld attachment in each of 129 sessions. No
    // owner commits follow: maintenance alone must exhaust the session scan.
    let mut sessions = Vec::new();
    for index in 0..129 {
        let handles = SessionDeleteBlobHandles {
            factory: Arc::clone(&law.handles.factory),
            reopen: Arc::clone(&law.handles.reopen),
            probe: Arc::clone(&law.handles.probe),
            attachments: Arc::clone(&law.handles.attachments),
        };
        let other = HoldLaw::new(
            handles,
            &format!("attachment-session-{index:03}"),
            crate::Retention::UntilGc,
        )
        .await;
        let reference = other.put(format!("session-page-{index}").as_bytes()).await;
        let input = other.enqueue("withdrawn", &[&reference]).await;
        other.end_staging(&[&reference]).await;
        other
            .view
            .cancel_pending_turn_input(input.input_id.as_str())
            .await
            .expect("withdraw");
        other.view.vacuum().await.expect("queue the ended holder");
        assert!(
            other.session_holds(&reference).await,
            "vacuum publishes continuation"
        );
        sessions.push((other, reference));
    }
    law.reopen().await;
    for (session, reference) in &mut sessions {
        session.reopen_on(Arc::clone(&law.handles.factory)).await;
        assert!(session.session_holds(reference).await);
    }
    let mut remaining = 129;
    // Two 128-session pages suffice for 129 sessions.
    for _ in 0..2 {
        law.handles
            .factory
            .gc_unreachable()
            .await
            .expect("advance the session scan");
        law.reopen().await;
        let mut next = 0;
        for (session, reference) in &mut sessions {
            session.reopen_on(Arc::clone(&law.handles.factory)).await;
            next += usize::from(session.session_holds(reference).await);
        }
        assert!(
            next < remaining || remaining == 0,
            "maintenance makes durable progress"
        );
        remaining = next;
        assert!(law.session_holds(&history).await);
        if remaining == 0 {
            break;
        }
    }
    assert_eq!(
        remaining, 0,
        "maintenance eventually exhausts all 129 sessions"
    );
    law.reclaim().await;
    for (session, reference) in &sessions {
        assert!(session.bytes_absent(reference).await);
    }
    law.facade
        .read(&history)
        .await
        .expect("the retained neighbor stays readable");
}

async fn held_count(law: &HoldLaw, references: &[crate::AttachmentRef]) -> usize {
    let mut held = 0;
    for reference in references {
        held += usize::from(law.session_holds(reference).await);
    }
    held
}
