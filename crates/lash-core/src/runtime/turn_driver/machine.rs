use super::*;
use crate::facade_support::SessionGraphFacadeOps as _;
use std::collections::BTreeSet;

/// Retains the messages at the last model call by shared identity. An
/// append-only machine names its reply from the witnessed extension, without
/// collecting or searching ids from preceding turns. A deliberate history
/// rewrite reconciles ids against that same snapshot.
#[derive(Debug, Default)]
pub(in crate::runtime) struct ProtocolReplyTracker {
    at_last_model_call: Option<crate::MessageSequence>,
}

impl ProtocolReplyTracker {
    /// Later runs retain the predecessor's last model-call boundary.
    pub(super) fn mark_run_start(&mut self, messages: &crate::MessageSequence) {
        if self.at_last_model_call.is_none() {
            self.mark_model_call(messages);
        }
    }

    pub(super) fn mark_model_call(&mut self, messages: &crate::MessageSequence) {
        self.at_last_model_call = Some(messages.clone());
    }

    /// Ids of assistant messages added since the last model call. A suffix is
    /// used only when its base and earlier delta agree by construction; its
    /// position alone never establishes message identity.
    pub(super) fn terminal_output(
        &self,
        messages: &crate::MessageSequence,
        graph: &crate::SessionGraph,
    ) -> Vec<String> {
        if let Some(before) = &self.at_last_model_call
            && let Some(tail) = before.preserved_extension_delta(messages)
        {
            let read = graph.read_model();
            if let Some(prefix_len) = before.base_prefix_len_in(&read.messages) {
                let mut prefix = read.messages;
                prefix.truncate(prefix_len);
                let base = crate::MessageSequence::from_base(prefix);
                if let Some(prior_delta) = base.preserved_extension_delta(before) {
                    let before_ids: BTreeSet<_> = prior_delta
                        .iter()
                        .filter(|message| message.role == MessageRole::Assistant)
                        .map(|message| message.id.as_str())
                        .collect();
                    return tail
                        .iter()
                        .filter(|message| message.role == MessageRole::Assistant)
                        .filter(|message| {
                            !before_ids.contains(message.id.as_str())
                                && !graph.contains_active_assistant_message_id_before(
                                    &message.id,
                                    prefix_len,
                                )
                        })
                        .map(|message| message.id.clone())
                        .collect();
                }
            }
        }
        let before: BTreeSet<_> = self
            .at_last_model_call
            .iter()
            .flat_map(crate::MessageSequence::iter)
            .filter(|message| message.role == MessageRole::Assistant)
            .map(|message| message.id.as_str())
            .collect();
        messages
            .iter()
            .filter(|message| message.role == MessageRole::Assistant)
            .filter(|message| !before.contains(message.id.as_str()))
            .map(|message| message.id.clone())
            .collect()
    }
}

impl RuntimeTurnDriver<'_> {
    pub(super) async fn apply_progress_boundary(
        &mut self,
        messages: crate::MessageSequence,
        event_delta: Vec<crate::SessionHistoryRecord>,
        protocol_iteration: usize,
    ) -> Result<(), RuntimeError> {
        let boundary = self
            .turn_pipeline
            .progress_boundary(
                &mut self.session,
                self.policy.policy.clone(),
                self.turn_index,
                messages,
                event_delta,
            )
            .await?;
        self.trace_protocol_steps(&boundary.protocol_events, protocol_iteration);
        Ok(())
    }

    /// Applies the records the machine's `Done` delivered to the turn's
    /// commit draft, tracing each protocol step as a progress boundary does.
    pub(super) fn apply_final_event_delta(
        &mut self,
        event_delta: Vec<crate::SessionHistoryRecord>,
        protocol_iteration: usize,
    ) {
        let protocol_events = self.turn_pipeline.apply_event_delta(event_delta);
        self.trace_protocol_steps(&protocol_events, protocol_iteration);
    }

    /// One `protocol_step` record per protocol event the turn records, in
    /// its order. A takeover re-applies its predecessor's boundaries
    /// untraced: their steps were traced by the owner that delivered them.
    fn trace_protocol_steps(&self, events: &[crate::ProtocolEvent], protocol_iteration: usize) {
        for event in events {
            self.emit_trace(protocol_iteration, || protocol_step_trace_event(event));
        }
    }
}
