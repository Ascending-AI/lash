//! A durable session commit's `Committed` observation (ADR 0002, FIG-5100,
//! FIG-5251).
//!
//! Every commit that moves a session's head (a turn's `turn.commit`, with a
//! frame switch's among them, and a `session.command`, a compaction's among
//! them) is published to the live replay store once the owner's commit is
//! acknowledged, by the node that made it: `Committed { base_revision, entries }`
//! at the head's revision, carrying the transcript entries the commit added to
//! what the session's subscribers held, after an `AgentFrameSwitched` when
//! it opened a frame. Only `Committed` settles the provisional activity the
//! turn streamed before it.
//!
//! The publication follows the commit, so an owner lost between the two,
//! or one whose commit's acknowledgement was lost, publishes nothing. Before
//! each pass an owner announces the durable head it finds unpublished by its
//! node: a `Committed` at the head with no entries over the head itself. A
//! subscriber that holds the head skips it as a redelivery; one that holds
//! an earlier revision cannot apply it and rebuilds from the durable head,
//! as a replay gap.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use lash_core_store::transcript::{EntryId, TranscriptDecoders};
use lash_sansio::sync::MutexExt as _;

use crate::{
    FrameNodeId, LashRuntime, LiveReplayEventDraft, LiveReplayStore, SessionId,
    SessionObservationEventPayload, SessionRevision, TurnId,
};

/// The durable heads this node published, per session.
#[derive(Default)]
pub(in crate::runtime) struct PublishedHeads(Mutex<HashMap<SessionId, SessionRevision>>);

impl PublishedHeads {
    fn note(&self, session: &SessionId, revision: SessionRevision) {
        let mut heads = self.0.lock_recover();
        let held = heads.entry(session.clone()).or_insert(revision);
        *held = (*held).max(revision);
    }

    fn holds(&self, session: &SessionId, revision: SessionRevision) -> bool {
        self.0
            .lock_recover()
            .get(session)
            .is_some_and(|held| *held >= revision)
    }
}

/// What a session's subscribers hold before a commit, read off the runtime
/// opened at the committed head: its revision, its current frame and the
/// entries of its transcript.
pub(in crate::runtime) struct CommitBase {
    session: SessionId,
    store: crate::store::SessionStore,
    revision: SessionRevision,
    frame: Option<FrameNodeId>,
    entries: HashSet<EntryId>,
    decoders: TranscriptDecoders,
}

impl CommitBase {
    /// The base `runtime`, opened at the session's committed head, stands
    /// on; `None` for a runtime with no store, which commits nothing.
    pub(in crate::runtime) fn of(runtime: &LashRuntime) -> Option<Self> {
        let store = runtime.services.store.clone()?;
        let decoders = runtime.services.plugins.transcript_decoders();
        let entries = crate::SessionReadView::recorded_from_runtime_state(&runtime.state)
            .with_transcript_decoders(decoders.clone())
            .transcript()
            .map(|transcript| {
                transcript
                    .into_entries()
                    .into_iter()
                    .map(|entry| entry.entry_id)
                    .collect()
            })
            .unwrap_or_default();
        Some(Self {
            session: runtime.session_id().clone(),
            store,
            revision: SessionRevision::from_runtime(runtime),
            frame: runtime.state.current_frame_node_id.clone(),
            entries,
            decoders,
        })
    }

    /// Publish the commit past this base to `live`, once the owner's commit
    /// is acknowledged: the durable head's revision with the entries it added,
    /// addressed to `turn` when a turn made it. A head still at the base
    /// committed nothing, and publishes nothing.
    pub(in crate::runtime) async fn publish(
        &self,
        live: &dyn LiveReplayStore,
        published: &PublishedHeads,
        turn: Option<&TurnId>,
    ) {
        let loaded = match crate::store::load_session_window_state(
            &self.store,
            crate::store::WindowSelector::Current,
        )
        .await
        {
            Ok(Some(loaded)) => loaded,
            Ok(None) => return,
            Err(error) => {
                tracing::warn!(session = %self.session, %error, "a commit's head was not read for its publication");
                return;
            }
        };
        let revision = crate::runtime::observation::observation_revision(&loaded.state);
        if revision <= self.revision {
            return;
        }
        let entries = crate::SessionReadView::recorded_from_runtime_state(&loaded.state)
            .with_transcript_decoders(self.decoders.clone())
            .transcript()
            .map(|transcript| {
                transcript
                    .into_entries()
                    .into_iter()
                    .filter(|entry| !self.entries.contains(&entry.entry_id))
                    .collect()
            })
            .unwrap_or_default();
        let mut drafts = Vec::with_capacity(2);
        if loaded.state.current_frame_node_id != self.frame
            && let Some(frame_id) = loaded.state.current_frame_node_id.clone()
        {
            drafts.push(LiveReplayEventDraft::new(
                None::<TurnId>,
                SessionObservationEventPayload::AgentFrameSwitched {
                    frame_id: frame_id.into_inner(),
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
pub(in crate::runtime) async fn announce_head(
    backend: &crate::Backend,
    live: &dyn LiveReplayStore,
    published: &PublishedHeads,
    session: &SessionId,
) {
    let factory = backend.session_store_factory();
    let head = match crate::SessionCommitStore::load_session_head_meta(factory.as_ref(), session)
        .await
    {
        Ok(Some(head)) => head,
        Ok(None) => return,
        Err(error) => {
            tracing::warn!(%session, %error, "a session's head was not read for its announcement");
            return;
        }
    };
    let revision = SessionRevision::of_durable_head(&head);
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
            tracing::warn!(%session, %error, "a commit's observation was not published");
        }
    }
}
