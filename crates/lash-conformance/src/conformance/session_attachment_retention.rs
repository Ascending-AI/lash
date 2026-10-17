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
use lash_sansio::SessionId;

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
                        Some(lash_sansio::PartAttachment {
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
