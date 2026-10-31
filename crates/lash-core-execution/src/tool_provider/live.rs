use super::*;

impl AttemptContext<'_> {
    /// Live facts for this admitted call, outside its recorded attempt.
    pub fn live(&self) -> crate::ToolLivePublisher {
        self.live.clone()
    }

    #[cfg(any(test, feature = "testing"))]
    pub fn with_tool_live(mut self, lane: &crate::ToolLive) -> Self {
        self.live = lane.bind(crate::CallAddress {
            owner: self.owner.clone(),
            call_id: self.call_id.clone(),
            attempt: self.attempt_number,
        });
        self
    }
}

pub(super) fn bind(context: &ToolContext<'_>) -> crate::ToolLivePublisher {
    context.runtime_execution_context.as_ref().map_or_else(
        || {
            crate::ToolLive::default().bind(crate::CallAddress {
                owner: context.owner.clone(),
                call_id: context.call_id.clone(),
                attempt: context.attempt_number,
            })
        },
        |runtime| {
            runtime.tool_live.bind(crate::CallAddress {
                owner: context.owner.clone(),
                call_id: context.call_id.clone(),
                attempt: context.attempt_number,
            })
        },
    )
}
