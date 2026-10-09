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
//! the commit's observation follows it, so an owner lost between the two,
//! or one whose commit's acknowledgement was lost, publishes nothing. A
//! reader on the committing node waits for a publication in flight before
//! it calls the head it trails a gap ([`PublishedHeads::settled`]). Before
//! each pass an owner announces the durable head it finds unpublished by its
//! node: a `Committed` at the head with no entries over the head itself. A
//! subscriber that holds the head skips it as a redelivery; one that holds
//! an earlier revision cannot apply it and rebuilds from the durable head,
//! as a replay gap.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

use lash_core_store::transcript::{EntryId, TranscriptDecoders};
use lash_sansio::sync::MutexExt as _;

use crate::{
    FrameNodeId, LashRuntime, LiveReplayEventDraft, LiveReplayStore, SessionId,
    SessionObservationEventPayload, SessionRevision, TurnId,
};

/// The durable heads this node published, per session, and the commits it
/// is still publishing.
///
/// A commit is durable before its `Committed` is published, so a reader that
/// compares its cursor with the durable head inside that window finds the
/// head ahead of everything the replay holds. The commit's node marks the
/// window ([`committing`](Self::committing)); a reader on the node waits for
/// it to close ([`settled`](Self::settled)) before it calls that a gap
/// (FIG-5605).
#[derive(Default)]
pub struct PublishedHeads {
    heads: Mutex<HashMap<SessionId, SessionRevision>>,
    /// The head each commit still to be published stands on, per session.
    committing: Mutex<HashMap<SessionId, Vec<SessionRevision>>>,
    settled: tokio::sync::Notify,
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
    pub(in crate::runtime) fn committing(self: &Arc<Self>, base: &CommitBase) -> CommitInFlight {
        self.committing
            .lock_recover()
            .entry(base.session.clone())
            .or_default()
            .push(base.revision);
        CommitInFlight {
            published: Arc::clone(self),
            session: base.session.clone(),
            base: base.revision,
        }
    }

    /// Wait until this node is publishing no commit that could have moved
    /// `session`'s head to `durable`: one over a head before it.
    ///
    /// A commit over the head a reader holds has not landed, so a reader
    /// inside the pass that will make it never waits on that pass. The wait
    /// ends with the publication's attempt, whatever its result; the reader
    /// judges the replay again then.
    pub async fn settled(&self, session: &SessionId, durable: SessionRevision) {
        loop {
            let settled = self.settled.notified();
            tokio::pin!(settled);
            settled.as_mut().enable();
            let publishing = self
                .committing
                .lock_recover()
                .get(session)
                .is_some_and(|bases| bases.iter().any(|base| *base < durable));
            if !publishing {
                return;
            }
            settled.await;
        }
    }
}

/// One commit this node is publishing; dropping it ends the wait of every
/// reader held on it.
pub(in crate::runtime) struct CommitInFlight {
    published: Arc<PublishedHeads>,
    session: SessionId,
    base: SessionRevision,
}

impl Drop for CommitInFlight {
    fn drop(&mut self) {
        let mut committing = self.published.committing.lock_recover();
        if let Some(bases) = committing.get_mut(&self.session) {
            if let Some(index) = bases.iter().position(|base| *base == self.base) {
                bases.swap_remove(index);
            }
            if bases.is_empty() {
                committing.remove(&self.session);
            }
        }
        drop(committing);
        self.published.settled.notify_waiters();
    }
}

/// What a session's subscribers hold before a commit, read off the runtime
/// opened at the committed head: its revision, its current frame and the
/// entries of its transcript.
pub(in crate::runtime) struct CommitBase {
    session: SessionId,
    store: crate::store::SessionStore,
    revision: SessionRevision,
    frame: FrameNodeId,
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
            frame: runtime
                .state
                .current_frame_node_id
                .clone()
                .unwrap_or_else(|| runtime.state.initial_frame_node_id()),
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
                tracing::warn!(session_id = %self.session, %error, "a commit's head was not read for its publication");
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
        if let Some(frame_id) = loaded.state.current_frame_node_id.clone()
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
            tracing::warn!(session_id = %session, %error, "a session's head was not read for its announcement");
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
            tracing::warn!(session_id = %session, %error, "a commit's observation was not published");
        }
    }
}
