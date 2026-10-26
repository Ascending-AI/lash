//! A durable session commit's `Committed` observation (ADR 0002, FIG-5100,
//! FIG-5251).
//!
//! Every commit that moves a session's head (a turn's `turn.commit`, with a
//! frame switch's among them, and a `session.command`, a compaction's among
//! them) is published to the live replay store once the owner's commit is
//! acknowledged, by the node that made it: `Committed { base_revision, entries }`
//! at the head's revision, carrying the transcript entries the commit added to
//! what the session's subscribers held, after an `AgentFrameSwitched` naming
//! the commit when it left the frame they held for another. A head no
//! commit has given a graph stands on the session's initial frame, so the
//! commit that opens that frame switches nothing. Only `Committed` settles
//! the provisional activity the turn streamed before it.
//!
//! A turn's own activity is published before its commit (FIG-5507); only
//! its outcome, held for the commit (ADR 0122, FIG-5800), and the commit's
//! observation follow it, so an owner lost between the two, or one whose
//! commit's acknowledgement was lost, publishes neither. A
//! reader on the committing node waits for a publication in flight before
//! it calls the head it trails a gap ([`PublishedHeads::window`]). Before
//! each pass an owner announces the durable head it finds unpublished by its
//! node: a `Committed` at the head with no entries over the head itself. A
//! subscriber that holds the head skips it as a redelivery; one that holds
//! an earlier revision cannot apply it and rebuilds from the durable head,
//! as a replay gap.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub use super::publication_window::{PublicationMark, PublicationMarks, PublicationWindow};

use lash_core_store::transcript::{SessionTranscript, TranscriptDecoders};
use lash_protocol_types::sync::MutexExt as _;

use crate::FrameNodeId;
use crate::LashRuntime;
use crate::LiveReplayEventDraft;
use crate::LiveReplayStore;
use crate::SessionObservationEventPayload;
use crate::SessionRevision;
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::TurnId;

/// The durable heads this node published, per session, and the commits it
/// is still publishing.
#[derive(Default)]
pub struct PublishedHeads {
    heads: Mutex<HashMap<SessionId, SessionRevision>>,
    committing: Arc<PublicationMarks<SessionId, SessionRevision>>,
}

impl PublishedHeads {
    fn note(&self, session: &SessionId, revision: SessionRevision) {
        let mut heads = self.heads.lock_recover();
        let held = heads.entry(session.clone()).or_insert(revision);
        *held = (*held).max(revision);
    }

    fn holds(&self, session: &SessionId, revision: SessionRevision) -> bool {
        self.heads
            .lock_recover()
            .get(session)
            .is_some_and(|held| *held >= revision)
    }

    /// Mark a commit over `base` as this node's to publish, until the
    /// returned mark is dropped: once its publication was attempted, or the
    /// commit was given up.
    pub(in crate::runtime) fn committing(
        &self,
        base: &CommitBase,
    ) -> PublicationMark<SessionId, SessionRevision> {
        self.committing.hold(&base.session, base.revision)
    }

    /// The publication window of `session`'s head `durable`, read just now.
    pub fn window(
        &self,
        session: &SessionId,
        durable: SessionRevision,
    ) -> PublicationWindow<SessionId, SessionRevision> {
        self.committing.window(session, durable)
    }
}

/// What a session's subscribers hold before a commit, read off the runtime
/// opened at the committed head: its revision, its current frame and the
/// graph whose active path is its transcript.
pub(in crate::runtime) struct CommitBase {
    session: SessionId,
    revision: SessionRevision,
    frame: FrameNodeId,
    graph: crate::SessionGraph,
    decoders: TranscriptDecoders,
}

impl CommitBase {
    /// The base `runtime`, opened at the session's committed head, stands
    /// on; `None` for a runtime with no store, which commits nothing.
    pub(in crate::runtime) fn of(runtime: &LashRuntime) -> Option<Self> {
        runtime.services.store.as_ref()?;
        Some(Self {
            session: runtime.session_id().clone(),
            revision: crate::runtime::observation::observation_revision(&runtime.state),
            frame: runtime
                .state
                .current_frame_node_id
                .clone()
                .unwrap_or_else(|| runtime.state.initial_frame_node_id()),
            graph: runtime.state.session_graph.clone(),
            decoders: runtime.services.plugins.transcript_decoders(),
        })
    }

    pub(in crate::runtime) async fn publish_retained(
        &self,
        live: &dyn LiveReplayStore,
        published: &PublishedHeads,
        turn: Option<&TurnId>,
        head: &super::head::SessionHead,
    ) {
        self.publish_state(live, published, turn, head.state())
            .await;
    }

    pub(in crate::runtime) async fn publish_state(
        &self,
        live: &dyn LiveReplayStore,
        published: &PublishedHeads,
        turn: Option<&TurnId>,
        state: &crate::RuntimeSessionState,
    ) {
        let revision = crate::runtime::observation::observation_revision(state);
        if revision <= self.revision {
            return;
        }
        let entries =
            SessionTranscript::added_since(&self.graph, &state.session_graph, &self.decoders)
                .map(SessionTranscript::into_entries)
                .unwrap_or_default();
        let mut drafts = Vec::with_capacity(2);
        if let Some(frame_id) = state.current_frame_node_id.clone()
            && frame_id != self.frame
        {
            drafts.push(LiveReplayEventDraft::new(
                None::<TurnId>,
                SessionObservationEventPayload::AgentFrameSwitched {
                    frame_id: frame_id.into_inner(),
                    commit: Some(revision),
                },
            ));
        }
        drafts.push(LiveReplayEventDraft::new(
            turn,
            SessionObservationEventPayload::Committed {
                base_revision: self.revision,
                entries,
            },
        ));
        publish(live, published, &self.session, revision, drafts).await;
    }
}

/// Announce `session`'s durable head when this node has not published it:
/// what an owner does before each pass, so a commit whose publication was
/// lost still reaches the session's subscribers.
pub(in crate::runtime) async fn announce_retained(
    live: &dyn LiveReplayStore,
    published: &PublishedHeads,
    head: &super::head::SessionHead,
) {
    let session = &head.state().session_id;
    let revision = crate::runtime::observation::observation_revision(head.state());
    if published.holds(session, revision) {
        return;
    }
    let draft = LiveReplayEventDraft::new(
        None::<TurnId>,
        SessionObservationEventPayload::Committed {
            base_revision: revision,
            entries: Vec::new(),
        },
    );
    publish(live, published, session, revision, vec![draft]).await;
}

async fn publish(
    live: &dyn LiveReplayStore,
    published: &PublishedHeads,
    session: &SessionId,
    revision: SessionRevision,
    drafts: Vec<LiveReplayEventDraft>,
) {
    match live.publish(session, revision, drafts).await {
        Ok(_) => published.note(session, revision),
        // The live stream is best effort: the next pass announces the head
        // again.
        Err(error) => {
            tracing::warn!(session_id = %session, %error, "a commit's observation was not published");
        }
    }
}
