//! Incremental prompt-resume validation over ordered tool-pairing facts.
use super::*;
use std::collections::HashSet;
use std::sync::Mutex;

#[cfg(test)]
thread_local! {
    pub(super) static MESSAGE_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PairingFact {
    Call(crate::ToolCallId),
    Result(crate::ToolCallId),
    Invalid,
}

fn facts(message: &Message) -> Vec<PairingFact> {
    #[cfg(test)]
    MESSAGE_VISITS.with(|visits| visits.set(visits.get() + 1));
    // A cell record answers its assistant context; none of its parts awaits
    // a tool result. Reasoning and other non-tool parts never affect pairing.
    if matches!(
        message.origin,
        Some(MessageOrigin::TurnOutput {
            cell_id: Some(_),
            ..
        })
    ) {
        return Vec::new();
    }
    message
        .parts
        .iter()
        .filter_map(|part| match part.kind() {
            PartKind::ToolCall => Some(match (message.role, part.call_id()) {
                (MessageRole::Assistant, Some(id)) => PairingFact::Call(id.clone()),
                _ => PairingFact::Invalid,
            }),
            PartKind::ToolResult => Some(match (message.role, part.call_id()) {
                (MessageRole::User, Some(id)) => PairingFact::Result(id.clone()),
                _ => PairingFact::Invalid,
            }),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Default)]
struct PairingState {
    seen: HashSet<crate::ToolCallId>,
    completed: HashSet<crate::ToolCallId>,
    refused: bool,
    safe: usize,
}

impl PairingState {
    fn append(&mut self, facts: &[PairingFact], len: usize) -> usize {
        if self.refused {
            return self.safe;
        }
        for fact in facts {
            let accepted = match fact {
                PairingFact::Call(id) => self.seen.insert(id.clone()),
                PairingFact::Result(id) => {
                    self.seen.contains(id) && self.completed.insert(id.clone())
                }
                PairingFact::Invalid => false,
            };
            if !accepted {
                self.refused = true;
                return self.safe;
            }
        }
        if self.seen.len() == self.completed.len() {
            self.safe = len;
        }
        self.safe
    }
}

#[derive(Debug, Default)]
struct ValidationLog {
    state: PairingState,
    entries: Vec<(Vec<PairingFact>, usize)>,
}

impl ValidationLog {
    fn append(&mut self, facts: Vec<PairingFact>) -> usize {
        let safe = self.state.append(&facts, self.entries.len() + 1);
        self.entries.push((facts, safe));
        safe
    }

    fn prefix(&self, len: usize) -> Self {
        let mut prefix = Self::default();
        for (facts, _) in &self.entries[..len] {
            prefix.append(facts.clone());
        }
        prefix
    }
}

/// An immutable validation cursor. Snapshots share an append-only index of
/// call identities and per-message safe cuts, without copying either on append.
/// A competing extension adopts only equal pairing facts; a divergent branch
/// rebuilds its own prefix under the same lock that classified the divergence.
#[derive(Clone, Debug, Default)]
pub(super) struct ResumeSafety {
    log: Arc<Mutex<ValidationLog>>,
    len: usize,
    safe: usize,
}

impl ResumeSafety {
    pub(super) fn len(&self) -> usize {
        self.len
    }

    pub(super) fn safe_len(&self) -> usize {
        self.safe
    }

    pub(super) fn append(&mut self, message: &Message) {
        let facts = facts(message);
        let mut log = self.log.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((previous, safe)) = log.entries.get(self.len) {
            if *previous == facts {
                self.safe = *safe;
                self.len += 1;
                return;
            }
            let mut branch = log.prefix(self.len);
            self.safe = branch.append(facts);
            drop(log);
            self.log = Arc::new(Mutex::new(branch));
        } else {
            self.safe = log.append(facts);
        }
        self.len += 1;
    }

    pub(super) fn extend<'a>(&mut self, messages: impl IntoIterator<Item = &'a Message>) {
        for message in messages {
            self.append(message);
        }
    }
}

pub fn messages_are_prompt_resume_safe<'a>(
    messages: impl IntoIterator<Item = &'a Message>,
) -> bool {
    let mut count = 0;
    let safe = prompt_resume_safe_len(messages.into_iter().inspect(|_| count += 1));
    safe == count
}

/// Length of the longest prefix in which every tool call is answered once,
/// after its call, with the expected assistant/user roles.
pub fn prompt_resume_safe_len<'a>(messages: impl IntoIterator<Item = &'a Message>) -> usize {
    let mut state = PairingState::default();
    for (index, message) in messages.into_iter().enumerate() {
        state.append(&facts(message), index + 1);
        if state.refused {
            break;
        }
    }
    state.safe
}

/// The cache owner certifies an unchanged prefix when it constructs an
/// extension, including when the message buffer grew or adopted timestamps.
/// Once bound, a cache reused with another buffer must validate that input.
#[derive(Debug, Default)]
pub(super) struct BaseSafety {
    base: Option<AppendVec<Message>>,
    cursor: ResumeSafety,
}

impl BaseSafety {
    pub(super) fn extending(&self) -> Self {
        Self {
            base: None,
            cursor: self.cursor.clone(),
        }
    }

    pub(super) fn for_base(&mut self, base: &AppendVec<Message>) -> ResumeSafety {
        if self.cursor.len() > base.len()
            || self
                .base
                .as_ref()
                .is_some_and(|previous| !AppendVec::ptr_eq(previous, base))
        {
            self.cursor = ResumeSafety::default();
        }
        self.cursor.extend(&base.as_slice()[self.cursor.len()..]);
        self.base = Some(base.clone());
        self.cursor.clone()
    }
}
