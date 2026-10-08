//! Delta framing (FIG-5098): the turn's stream deltas reach the host, and
//! through it the live replay store, coalesced into short frames.
//!
//! A provider streams about one delta per token. Publishing each one as its
//! own event costs a live replay position, a store write and a subscriber
//! notification per token, and fills the replay window in seconds. So each
//! lane keeps at most one open frame: the deltas of one block and kind of one
//! physical turn, in order. Coalescing happens here, before the store assigns
//! positions, so a frame is one event with one position.
//!
//! - **The first delta of a block is published at once** (unless the host
//!   turned that off), so time to first token never waits on a frame.
//! - **A frame is due its interval after it opened.** The publisher takes it
//!   then, or later if the host is still taking events queued ahead of it;
//!   until the publisher takes it, it keeps absorbing the block's deltas.
//!   That also bounds a lagging host's backlog: its deltas pile into the
//!   frame it has not taken yet rather than into the queue.
//! - **A frame is cut early** by its size cap, by a delta of another block,
//!   kind, turn or replay key on its lane, and by any non-delta event on
//!   either lane, which is queued behind it. So a frame never moves a delta
//!   across another event of its lane.
//! - **A frame names the observations it covers.** A delta activity is named
//!   `{key}#{ordinal}` by the observation that yielded it; a frame of them is
//!   named `{key}#{first}..{last}`
//!   ([`TurnActivityId::observed_range`](crate::TurnActivityId::observed_range)).
//!   A frame is cut by every other activity of its lane, so every activity
//!   its key yielded inside that range is one of its deltas. A redrive that
//!   republishes the same observations, framed alike or not, re-derives
//!   names inside the ranges the store already delivered, which is how the
//!   store drops them (`replay::activity_spans`).
//!
//! Every parameter is the host's
//! ([`DeltaCoalescing`](crate::runtime::DeltaCoalescing)); coalescing off
//! queues every delta as its own event.

use crate::llm::types::{StreamBlockEvent, StreamBlockKind};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use super::RuntimeStreamEvent;
use super::{Lane, Observation, lane};
use crate::runtime::DeltaCoalescing;
use crate::session_model::SessionStreamEvent;
use crate::{TurnActivity, TurnActivityId, TurnEvent, TurnId};

/// How a turn frames its stream deltas: the clock frames are timed on and
/// the host's terms
/// ([`RuntimeControlConfig::delta_coalescing`](crate::RuntimeControlConfig::delta_coalescing)).
#[derive(Clone)]
pub(in crate::runtime) struct DeltaFraming {
    pub(in crate::runtime) clock: Arc<dyn crate::Clock>,
    pub(in crate::runtime) coalescing: DeltaCoalescing,
}

/// The open frame of each lane, and the block each lane last streamed.
#[derive(Default)]
pub(super) struct Frames {
    lanes: [LaneFrame; 2],
}

#[derive(Default)]
struct LaneFrame {
    open: Option<OpenFrame>,
    /// The block of the last delta queued on this lane: a delta of any other
    /// block is the first of its block.
    streaming: Option<BlockKey>,
}

#[derive(Clone, PartialEq, Eq)]
struct BlockKey {
    turn: Option<TurnId>,
    kind: StreamBlockKind,
    block_id: String,
}

/// A stream delta, as framing sees it.
struct Delta<'a> {
    lane: Lane,
    block: BlockKey,
    text: &'a str,
    /// An activity's correlation, replay key and ordinal. `None` on the
    /// session lane; an activity delta whose id names no observation is
    /// never framed.
    activity: Option<(&'a TurnActivityId, &'a str, u32)>,
}

struct OpenFrame {
    /// The first delta: the frame's event, turn and correlation.
    first: Observation,
    block: BlockKey,
    text: String,
    /// The activity frame's correlation, replay key and ordinal range.
    span: Option<ActivitySpan>,
    due: Instant,
}

struct ActivitySpan {
    correlation: TurnActivityId,
    key: String,
    first: u32,
    last: u32,
}

impl OpenFrame {
    fn absorbs(&self, delta: &Delta<'_>, max_frame_bytes: usize) -> bool {
        if self.block != delta.block || self.text.len() + delta.text.len() > max_frame_bytes {
            return false;
        }
        match (&self.span, delta.activity) {
            (None, None) => true,
            (Some(span), Some((correlation, key, ordinal))) => {
                span.correlation == *correlation && span.key == key && ordinal > span.last
            }
            _ => false,
        }
    }

    /// The frame as one event: its text, and for an activity the id of the
    /// range it covers.
    fn seal(self) -> Observation {
        let Observation { turn, mut event } = self.first;
        match &mut event {
            RuntimeStreamEvent::Session(SessionStreamEvent::StreamBlock(
                StreamBlockEvent::Delta { text, .. },
            )) => *text = self.text,
            RuntimeStreamEvent::Turn(TurnActivity {
                id,
                event: TurnEvent::StreamBlock(StreamBlockEvent::Delta { text, .. }),
                ..
            }) => {
                *text = self.text;
                if let Some(span) = self.span {
                    *id = TurnActivityId::observed_range(span.key, span.first, span.last);
                }
            }
            _ => {}
        }
        Observation { turn, event }
    }
}

fn classify(observation: &Observation) -> Option<Delta<'_>> {
    let turn = observation.turn.clone();
    let (kind, block, text, activity) =
        match &observation.event {
            RuntimeStreamEvent::Session(SessionStreamEvent::StreamBlock(
                StreamBlockEvent::Delta { kind, block, text },
            )) => (*kind, block, text.as_str(), None),
            RuntimeStreamEvent::Turn(TurnActivity {
                id,
                correlation_id,
                event: TurnEvent::StreamBlock(StreamBlockEvent::Delta { kind, block, text }),
            }) => (*kind, block, text.as_str(), Some((id, correlation_id))),
            _ => return None,
        };
    let activity = match activity {
        None => None,
        Some((id, correlation_id)) => {
            let (key, ordinals) = id.observed_span()?;
            Some((correlation_id, key, *ordinals.end()))
        }
    };
    Some(Delta {
        lane: lane(&observation.event),
        block: BlockKey {
            turn,
            kind,
            block_id: block.id.clone(),
        },
        text,
        activity,
    })
}

/// Whether `observation` is a stream delta: an activity delta whose id names
/// no observation is still a delta, though never framed.
fn is_delta(observation: &Observation) -> bool {
    matches!(
        &observation.event,
        RuntimeStreamEvent::Session(SessionStreamEvent::StreamBlock(
            StreamBlockEvent::Delta { .. }
        )) | RuntimeStreamEvent::Turn(TurnActivity {
            event: TurnEvent::StreamBlock(StreamBlockEvent::Delta { .. }),
            ..
        })
    )
}

/// What queueing one observation changed for the publisher.
pub(super) enum Queued {
    /// The event, or a frame cut before it, is ready for the publisher.
    Ready,
    /// A frame opened: the publisher must time it.
    Opened,
    /// The delta joined the open frame.
    Absorbed,
}

impl Frames {
    /// Queue `observation` onto `events`, framing it when it is a delta. A
    /// frame opened now falls due at `due`.
    pub(super) fn push(
        &mut self,
        events: &mut VecDeque<Observation>,
        observation: Observation,
        coalescing: &DeltaCoalescing,
        due: Instant,
    ) -> Queued {
        if coalescing.is_off() {
            events.push_back(observation);
            return Queued::Ready;
        }
        if !is_delta(&observation) {
            self.seal_all(events);
            events.push_back(observation);
            return Queued::Ready;
        }
        let Some(delta) = classify(&observation) else {
            // An activity delta no observation names: no range can name a
            // frame of it, so it is queued as it is.
            let lane = &mut self.lanes[lane(&observation.event) as usize];
            events.extend(lane.open.take().map(OpenFrame::seal));
            lane.streaming = None;
            events.push_back(observation);
            return Queued::Ready;
        };
        let lane = &mut self.lanes[delta.lane as usize];
        if let Some(open) = lane.open.as_mut()
            && open.absorbs(&delta, coalescing.max_frame_bytes())
        {
            open.text.push_str(delta.text);
            if let (Some(span), Some((_, _, ordinal))) = (&mut open.span, delta.activity) {
                span.last = ordinal;
            }
            return Queued::Absorbed;
        }
        events.extend(lane.open.take().map(OpenFrame::seal));
        if lane.streaming.as_ref() != Some(&delta.block) {
            lane.streaming = Some(delta.block.clone());
            if coalescing.first_delta_immediate() {
                // The first delta of its block is published at once.
                events.push_back(observation);
                return Queued::Ready;
            }
        }
        let frame = OpenFrame {
            block: delta.block,
            text: delta.text.to_owned(),
            span: delta
                .activity
                .map(|(correlation, key, ordinal)| ActivitySpan {
                    correlation: correlation.clone(),
                    key: key.to_owned(),
                    first: ordinal,
                    last: ordinal,
                }),
            due,
            first: observation,
        };
        lane.open = Some(frame);
        Queued::Opened
    }

    /// Queue every open frame, oldest first.
    pub(super) fn seal_all(&mut self, events: &mut VecDeque<Observation>) {
        while let Some(frame) = self.take_earliest() {
            events.push_back(frame.seal());
        }
    }

    /// Take the open frame due earliest when it is due at `now`, or `forced`.
    pub(super) fn take_due(&mut self, now: Instant, forced: bool) -> Option<Observation> {
        let due = self.next_due()?;
        (forced || due <= now)
            .then(|| self.take_earliest().map(OpenFrame::seal))
            .flatten()
    }

    /// When the next open frame falls due.
    pub(super) fn next_due(&self) -> Option<Instant> {
        self.lanes
            .iter()
            .filter_map(|lane| lane.open.as_ref().map(|frame| frame.due))
            .min()
    }

    pub(super) fn is_empty(&self) -> bool {
        self.lanes.iter().all(|lane| lane.open.is_none())
    }

    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.lanes.iter().filter(|lane| lane.open.is_some()).count()
    }

    fn take_earliest(&mut self) -> Option<OpenFrame> {
        self.lanes
            .iter_mut()
            .filter(|lane| lane.open.is_some())
            .min_by_key(|lane| lane.open.as_ref().map(|frame| frame.due))?
            .open
            .take()
    }
}
