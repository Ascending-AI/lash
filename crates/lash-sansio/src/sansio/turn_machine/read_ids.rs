use super::*;
use lash_protocol_types::Part;

impl<M: TurnProtocol> TurnMachine<M> {
    /// Bind observational read ids before the model round checkpoint is saved.
    pub fn bind_tool_call_ids(
        &mut self,
        ids: &std::collections::BTreeMap<
            lash_protocol_types::ToolCallId,
            lash_protocol_types::ToolCallId,
        >,
    ) {
        if ids.is_empty() {
            return;
        }
        let has_bound_id = |parts: &[Part]| {
            parts.iter().any(|part| match part {
                Part::ToolCall { call_id, .. } | Part::ToolResult { call_id, .. } => {
                    ids.contains_key(call_id)
                }
                _ => false,
            })
        };
        let rebind_parts = |parts: &mut std::sync::Arc<Vec<Part>>| {
            if has_bound_id(parts) {
                for part in std::sync::Arc::make_mut(parts) {
                    if let Part::ToolCall { call_id, .. } | Part::ToolResult { call_id, .. } = part
                        && let Some(bound) = ids.get(call_id)
                    {
                        *call_id = bound.clone();
                    }
                }
            }
        };
        // Unanswered tool calls have emitted no resumable progress boundary.
        // Rewrite only the turn's uncommitted tail, retaining its shared window.
        let rebind_messages = |messages: &MessageSequence| {
            let shared = self
                .window
                .as_ref()
                .map_or(0, |window| window.shared_messages(messages));
            let first = messages
                .iter()
                .enumerate()
                .skip(shared)
                .find_map(|(index, message)| has_bound_id(&message.parts).then_some(index))?;
            let mut tail: Vec<_> = messages.iter().skip(shared).cloned().collect();
            for message in &mut tail {
                rebind_parts(&mut message.parts);
            }
            let rebound = match &self.window {
                Some(window) => window.messages_then(shared, tail),
                None => MessageSequence::from_owned(tail),
            };
            Some((first, rebound))
        };
        if let Some((first, messages)) = rebind_messages(&self.messages) {
            self.content.rewrote(Sequence::Messages, first);
            self.messages = messages;
        }
        if let Some((first, messages)) = rebind_messages(&self.prompt_messages) {
            self.content.rewrote(Sequence::Prompt, first);
            self.prompt_messages = messages;
        }
        if let Some(first) = self
            .events
            .iter()
            .enumerate()
            .skip(self.progress_event_cursor)
            .find_map(|(index, event)| match event {
                SessionHistoryRecord::Conversation(record) if has_bound_id(&record.parts) => {
                    Some(index)
                }
                _ => None,
            })
        {
            let mut tail: Vec<_> = self.events.iter().skip(first).cloned().collect();
            for event in &mut tail {
                if let SessionHistoryRecord::Conversation(record) = event {
                    rebind_parts(&mut record.parts);
                }
            }
            self.events.replace_from(first, tail);
            self.content.rewrote(Sequence::Events, first);
        }
        if let MachineState::Waiting {
            work: PendingWork::WaitingForToolResults { wave, .. },
            ..
        } = &mut self.state
        {
            let rebind = |call: &mut PendingToolCall| {
                if let Some(id) = ids.get(&call.call_id) {
                    call.call_id = id.clone();
                }
            };
            match wave.as_mut() {
                crate::sansio::ToolWave::Round { calls } => calls.iter_mut().for_each(rebind),
                crate::sansio::ToolWave::Siblings { before, after, .. } => {
                    before.iter_mut().chain(after).for_each(rebind)
                }
                crate::sansio::ToolWave::Control { control, .. } => rebind(control),
            }
        }
    }
}
