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

impl crate::AttemptContext<'_> {
    /// Declare the one pending child against this admitted attempt.
    pub fn declare_start(
        &self,
        start: lash_execution_contracts::tool_intent::StartProcessIntent,
    ) -> Result<
        lash_execution_contracts::DeclaredStart,
        lash_execution_contracts::DeclaredStartRefused,
    > {
        if start.owner != self.owner().runtime_owner() {
            return Err(lash_execution_contracts::DeclaredStartRefused::ForeignOwner);
        }
        if self.completion_key().is_err() {
            return Err(lash_execution_contracts::DeclaredStartRefused::CompletionUnavailable);
        }
        let identity = self.intent_identity(0);
        Ok(lash_execution_contracts::declared_start::declared_start(
            start, identity,
        ))
    }
}
