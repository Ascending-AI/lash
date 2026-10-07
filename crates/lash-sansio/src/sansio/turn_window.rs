//! The committed session window a turn starts from (FIG-5206).
//!
//! A turn's messages and history begin with its session head's window, which
//! grows with every turn the session committed. A checkpoint names the window
//! by the host's pin and by how much of it each sequence still starts with,
//! and holds only what the turn added: its messages, its history records and
//! the part of a pending model request the window does not render. A restore
//! is handed the window again, read at the pinned head, so its cost is the
//! turn's, not the session's history.

use std::sync::Arc;

use crate::AppendVec;
use crate::llm::types::LlmMessage;
use crate::session_model::message::message_content_equal;
use crate::session_model::{BaseRenderCache, Message, MessageSequence, SessionHistoryRecord};

/// The host's name for a committed window, encoded by the host: the machine
/// only compares it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct TurnWindowPin(String);

impl TurnWindowPin {
    #[must_use]
    pub fn new(pin: String) -> Self {
        Self(pin)
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A committed window: its pin, its messages and its history records. The
/// window is immutable, so the same pin always reads back the same window.
pub struct TurnWindow<E = ()> {
    pin: TurnWindowPin,
    messages: AppendVec<Message>,
    events: AppendVec<SessionHistoryRecord<E>>,
    rendered: Arc<BaseRenderCache>,
}

impl<E> Clone for TurnWindow<E> {
    fn clone(&self) -> Self {
        Self {
            pin: self.pin.clone(),
            messages: self.messages.clone(),
            events: self.events.clone(),
            rendered: Arc::clone(&self.rendered),
        }
    }
}

impl<E> std::fmt::Debug for TurnWindow<E> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TurnWindow")
            .field("pin", &self.pin)
            .field("messages", &self.messages.len())
            .field("events", &self.events.len())
            .finish_non_exhaustive()
    }
}

impl<E: Clone> TurnWindow<E> {
    /// The window `pin` names: `messages` and `events`.
    #[must_use]
    pub fn new(
        pin: TurnWindowPin,
        messages: AppendVec<Message>,
        events: AppendVec<SessionHistoryRecord<E>>,
    ) -> Self {
        Self {
            pin,
            messages,
            events,
            rendered: Arc::new(BaseRenderCache::new()),
        }
    }

    /// Share `cache`, a render cache made for this window's messages.
    #[must_use]
    pub fn with_render_cache(mut self, cache: Arc<BaseRenderCache>) -> Self {
        self.rendered = cache;
        self
    }

    #[must_use]
    pub fn pin(&self) -> &TurnWindowPin {
        &self.pin
    }

    #[must_use]
    pub fn messages(&self) -> &AppendVec<Message> {
        &self.messages
    }

    #[must_use]
    pub fn events(&self) -> &AppendVec<SessionHistoryRecord<E>> {
        &self.events
    }

    /// The window's messages followed by `delta`, sharing the window's
    /// buffer and render.
    #[must_use]
    pub fn then(&self, delta: Vec<Message>) -> MessageSequence {
        MessageSequence::from_base_and_delta(self.messages.clone(), delta)
            .with_base_render_cache(Arc::clone(&self.rendered))
    }

    /// How many of `sequence`'s leading messages are the window's.
    pub(super) fn shared_messages(&self, sequence: &MessageSequence) -> usize {
        if sequence.has_base(&self.messages) {
            return self.messages.len();
        }
        self.messages
            .iter()
            .zip(sequence.iter())
            .take_while(|(window, message)| message_content_equal(*window, *message))
            .count()
    }

    /// How many of `rendered`'s leading model messages are the window's own
    /// render.
    pub(super) fn shared_render(&self, rendered: &[LlmMessage]) -> usize {
        self.render()
            .iter()
            .zip(rendered)
            .take_while(|(window, message)| window == message)
            .count()
    }

    /// The window's messages, rendered as a model prompt.
    pub(super) fn render(&self) -> &AppendVec<LlmMessage> {
        self.rendered.rendered(self.messages.as_slice())
    }

    /// The window's first `len` messages followed by `rest`.
    pub(super) fn messages_then(&self, len: usize, rest: Vec<Message>) -> MessageSequence {
        if len == self.messages.len() {
            return self.then(rest);
        }
        let mut base = self.messages.clone();
        base.truncate(len);
        MessageSequence::from_base_and_delta(base, rest)
    }
}
