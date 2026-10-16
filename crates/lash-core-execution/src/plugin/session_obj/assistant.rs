//! Assistant transforms and receipt stream replay.

use super::*;

impl PluginSession {
    pub async fn transform_assistant_stream(
        &self,
        address: &ModelCallAddress,
        chunk: String,
    ) -> Result<Vec<PluginOwned<AssistantStreamTransform>>, PluginError> {
        let mut current = chunk;
        let mut transforms = Vec::new();
        for registered in &self.capabilities().contributions.assistant_stream_hooks {
            let transform = (registered.hook)(AssistantStreamHookContext {
                session_id: address.session_id.clone(),
                address: address.clone(),
                plugin_config: self.admitted_plugin_config(),
                chunk: current.clone(),
            })
            .await?;
            current = transform.chunk.clone();
            transforms.push(PluginOwned {
                plugin_id: registered.identity.owner.clone(),
                value: transform,
            });
        }
        Ok(transforms)
    }

    /// Check every recorded key and stream-state format without invoking a callback.
    pub fn validate_assistant_response_plan(
        &self,
        plan: &crate::runtime::AssistantResponsePlan,
        stream_hook_states: &[crate::runtime::AssistantStreamHookState],
    ) -> Result<(), PluginError> {
        let callbacks = self.resolve_assistant_response_plan(plan)?;
        // Validate all recorded state before the first callback can run.
        for registered in &callbacks {
            if let Some(recorded) = stream_hook_states
                .iter()
                .find(|state| state.callback == registered.identity)
            {
                let declaration = self
                    .host
                    .factories()
                    .iter()
                    .find(|factory| factory.id() == registered.identity.owner)
                    .ok_or_else(|| self.unavailable_callback(&registered.identity))?
                    .plugin_declaration();
                if !declaration
                    .readable_stream_state_formats
                    .contains(&recorded.format)
                {
                    return Err(PluginError::Runtime(
                        StreamStateFormatRefusal {
                            callback: recorded.callback.clone(),
                            stored: recorded.format,
                            readable: declaration.readable_stream_state_formats,
                        }
                        .into_runtime_error(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn resolve_assistant_response_plan(
        &self,
        plan: &crate::runtime::AssistantResponsePlan,
    ) -> Result<Vec<&RegisteredHook<AssistantResponseHook>>, PluginError> {
        self.capabilities().composition.recorded().resolve_all(
            &plan.callbacks,
            &self.capabilities().contributions.assistant_response_hooks,
        )
    }

    pub async fn transform_assistant_response(
        &self,
        address: &ModelCallAddress,
        response: crate::llm::types::LlmResponse,
        plan: &crate::runtime::AssistantResponsePlan,
        stream_hook_states: &[crate::runtime::AssistantStreamHookState],
    ) -> Result<Vec<PluginOwned<AssistantResponseTransform>>, PluginError> {
        self.validate_assistant_response_plan(plan, stream_hook_states)?;
        let callbacks = self.resolve_assistant_response_plan(plan)?;
        let mut current = response;
        let mut transforms = Vec::new();
        for registered in callbacks {
            let stream_state = stream_hook_states
                .iter()
                .find(|recorded| recorded.callback == registered.identity);
            let transform = (registered.hook)(AssistantResponseHookContext {
                session_id: address.session_id.clone(),
                address: address.clone(),
                plugin_config: self.admitted_plugin_config(),
                response: current.clone(),
                stream_state: stream_state.map(|recorded| recorded.state.clone()),
                stream_state_format: stream_state.map(|recorded| recorded.format),
            })
            .await?;
            current = transform.response.clone();
            transforms.push(PluginOwned {
                plugin_id: registered.identity.owner.clone(),
                value: transform,
            });
        }
        Ok(transforms)
    }

    /// Runs every stream-finished hook, for a reset between streams or
    /// attempts. Its state is handed to each response callback that names it
    /// as its `stream_state_from`.
    pub async fn finish_assistant_stream(
        &self,
        address: &ModelCallAddress,
        reason: AssistantStreamFinishReason,
    ) -> Result<Vec<crate::runtime::AssistantStreamHookState>, PluginError> {
        let finished = self
            .capabilities()
            .contributions
            .assistant_stream_finished_hooks
            .iter()
            .collect::<Vec<_>>();
        self.finish_assistant_stream_with(address, &finished, reason)
            .await
    }

    /// The stream callbacks and the stream-finished callbacks a model call
    /// streams under, each in registration order: recorded in the call's
    /// [`AssistantStreamCapsule`](crate::runtime::AssistantStreamCapsule).
    pub fn assistant_stream_callbacks(
        &self,
    ) -> (Vec<PluginCallbackIdentity>, Vec<PluginCallbackIdentity>) {
        let contributions = &self.capabilities().contributions;
        (
            contributions
                .assistant_stream_hooks
                .iter()
                .map(|registered| registered.identity.clone())
                .collect(),
            contributions
                .assistant_stream_finished_hooks
                .iter()
                .map(|registered| registered.identity.clone())
                .collect(),
        )
    }

    /// Check that every callback `capsule` recorded resolves, without
    /// invoking one.
    pub fn validate_assistant_stream_capsule(
        &self,
        capsule: &crate::runtime::AssistantStreamCapsule,
    ) -> Result<(), PluginError> {
        self.resolve_assistant_stream_capsule(capsule).map(drop)
    }

    #[expect(
        clippy::type_complexity,
        reason = "the capsule's two resolved callback lists"
    )]
    fn resolve_assistant_stream_capsule(
        &self,
        capsule: &crate::runtime::AssistantStreamCapsule,
    ) -> Result<
        (
            Vec<&RegisteredHook<AssistantStreamHook>>,
            Vec<&RegisteredHook<AssistantStreamFinishedHook>>,
        ),
        PluginError,
    > {
        let recorded = self.capabilities().composition.recorded();
        let contributions = &self.capabilities().contributions;
        let (stream, finished) = capsule.finishing();
        Ok((
            recorded.resolve_all(stream, &contributions.assistant_stream_hooks)?,
            recorded.resolve_all(finished, &contributions.assistant_stream_finished_hooks)?,
        ))
    }

    /// Finish the stream a model call's outcome recorded in `capsule`, from
    /// the capsule and its receipt address, on this worker, for whatever reason it ended: the
    /// recorded stream-finished callbacks discard whatever they hold (an
    /// earlier stream's state, or the live stream of this very call), the
    /// recorded stream callbacks are handed the recorded chunks again in
    /// order, their transforms discarded (what they published, the call
    /// published as it streamed), and the stream-finished callbacks end the
    /// stream for the recorded reason. The states they return for a stream
    /// that produced a response are what the response callbacks derive
    /// with: the same on every worker and every derivation, given stream
    /// callbacks that are pure functions of the chunks they were handed.
    /// Exactly the callbacks [`AssistantStreamCapsule::finishing`] names run,
    /// and every one resolves before any runs.
    ///
    /// [`AssistantStreamCapsule::finishing`]: crate::runtime::AssistantStreamCapsule::finishing
    pub async fn replay_assistant_stream(
        &self,
        address: &ModelCallAddress,
        capsule: &crate::runtime::AssistantStreamCapsule,
    ) -> Result<Vec<crate::runtime::AssistantStreamHookState>, PluginError> {
        let (stream, finished) = self.resolve_assistant_stream_capsule(capsule)?;
        if finished.is_empty() {
            return Ok(Vec::new());
        }
        self.finish_assistant_stream_with(
            address,
            &finished,
            AssistantStreamFinishReason::AttemptReset,
        )
        .await?;
        for chunk in &capsule.chunks {
            let mut current = chunk.clone();
            for registered in &stream {
                current = (registered.hook)(AssistantStreamHookContext {
                    session_id: address.session_id.clone(),
                    address: address.clone(),
                    plugin_config: self.admitted_plugin_config(),
                    chunk: current,
                })
                .await?
                .chunk;
            }
        }
        let states = self
            .finish_assistant_stream_with(address, &finished, capsule.finish)
            .await?;
        Ok(if capsule.produced_response() {
            states
        } else {
            Vec::new()
        })
    }

    async fn finish_assistant_stream_with(
        &self,
        address: &ModelCallAddress,
        finished: &[&RegisteredHook<AssistantStreamFinishedHook>],
        reason: AssistantStreamFinishReason,
    ) -> Result<Vec<crate::runtime::AssistantStreamHookState>, PluginError> {
        let mut states = Vec::new();
        for registered in finished {
            let state = (registered.hook)(AssistantStreamFinishedContext {
                session_id: address.session_id.clone(),
                address: address.clone(),
                plugin_config: self.admitted_plugin_config(),
                reason,
            })
            .await?;
            let Some(state) = state else {
                continue;
            };
            for (response, _) in self
                .capabilities()
                .contributions
                .assistant_stream_state_pairs
                .iter()
                .filter(|(_, finished)| finished == &registered.identity)
            {
                states.push(crate::runtime::AssistantStreamHookState {
                    callback: response.clone(),
                    format: self
                        .host
                        .factories()
                        .iter()
                        .find(|factory| factory.id() == response.owner)
                        .ok_or_else(|| self.unavailable_callback(response))?
                        .plugin_declaration()
                        .stream_state_format,
                    state: state.clone(),
                });
            }
        }
        Ok(states)
    }
}
