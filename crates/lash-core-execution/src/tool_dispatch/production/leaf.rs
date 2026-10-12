//! A call's admission as a leaf of its owner's Run, and its answer once it
//! ended.
use super::*;
use crate::tool_dispatch::call_run::CallEnd;

fn encoding(message: String) -> SingletonRunError {
    crate::RuntimeEffectControllerError::new(crate::RuntimeErrorCode::RecordEncodingFailed, message)
        .into()
}

impl ProductionToolHandlers<'_> {
    /// The definition a tool leaf is admitted under: its recorded binding,
    /// its grant's, or the catalog's.
    pub(super) fn leaf_definition(
        &self,
        invocation: &crate::session::tool_execution::ToolInvocation,
    ) -> Option<crate::ToolDefinition> {
        invocation
            .recorded_binding
            .as_deref()
            .cloned()
            .or_else(|| {
                invocation
                    .execution_grant
                    .as_deref()
                    .map(|grant| crate::ToolDefinition {
                        manifest: grant.manifest().clone(),
                        contract: grant.contract().clone(),
                    })
            })
            .or_else(|| {
                self.context
                    .tool_catalog()
                    .tools
                    .iter()
                    .find(|definition| definition.manifest.id == invocation.tool_id)
                    .map(|entry| crate::ToolDefinition {
                        manifest: entry.manifest.clone(),
                        contract: entry.contract.as_ref().clone(),
                    })
            })
    }

    /// The call `invocation` runs as, its input registered with these
    /// handlers.
    pub(super) fn admit_leaf(
        &self,
        owner: &crate::EffectOpener,
        invocation: &crate::session::tool_execution::ToolInvocation,
        definition: crate::ToolDefinition,
        parent: Option<crate::RuntimeInvocation>,
        attribution: &crate::session::ToolObservationAttribution,
    ) -> Result<SingletonToolCall, SingletonRunError> {
        let source = invocation
            .execution_grant
            .as_deref()
            .and_then(|grant| grant.source_id.as_deref());
        let binding = self
            .context
            .dispatch()
            .composition
            .tool_run_binding(&invocation.tool_id, source)
            .map_err(crate::RuntimeEffectControllerError::from)?;
        if let Some(grant) = &invocation.execution_grant {
            self.context
                .dispatch()
                .composition
                .validate_tool_owner(&grant.owner)
                .map_err(crate::RuntimeEffectControllerError::from)?;
        }
        let mut attribution = attribution.clone();
        if invocation.issuing_language_node_id.is_some() {
            attribution.issuing_node_id = invocation.issuing_language_node_id.clone();
        }
        self.calls.lock_recover().insert(
            invocation.id.clone(),
            CallInput {
                attribution,
                definition: definition.clone(),
                pending: invocation.pending.clone(),
                grant: invocation.execution_grant.clone(),
                parent,
                binding: binding.clone(),
                facts: self.context.execution_facts().recorded_call(),
            },
        );
        Ok(SingletonToolCall {
            owner: owner.clone(),
            call_id: invocation.id.clone(),
            declaration: definition.manifest.declaration().clone(),
            tool_name: definition.manifest.name,
            arguments: invocation.args.clone(),
            binding,
            available: self.context.dispatch().composition.tool_run_revisions(),
            cancel: ExternalCancelPolicy::CancelExternalWork,
        })
    }

    /// What the machine or a consumer is answered with for `call_id`, ended
    /// as `end`.
    ///
    /// # Errors
    ///
    /// A capture or presentation that does not decode.
    pub(crate) fn completed_call(
        &self,
        call_id: &crate::ToolCallId,
        end: &CallEnd,
    ) -> Result<crate::sansio::CompletedToolCall, SingletonRunError> {
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or_else(|| encoding(format!("call {call_id} has no admitted preparation")))?;
        let (output, model_return, intent_outcomes) = match end {
            // An isolated final presents the descriptor of the process it
            // started; no ordinary body produced an output.
            CallEnd::Final {
                capture: SingletonCapture::Isolated { .. },
                presentation,
                ..
            } => {
                let output = ToolCallOutput::success(
                    decode::<serde_json::Value>(presentation).map_err(encoding)?,
                );
                let model_return =
                    crate::ModelToolReturn::from_output(prepared.call.tool_name.clone(), &output);
                (output, model_return, Vec::new())
            }
            CallEnd::Final {
                capture,
                presentation,
                ..
            } => {
                let presented: Presented = decode(presentation).map_err(encoding)?;
                let captured: Captured = decode(
                    capture
                        .output()
                        .ok_or_else(|| encoding("the final has no capture".to_owned()))?,
                )
                .map_err(encoding)?;
                let mut output = captured.output;
                super::super::attempt_coordinator::project_recorded_intent_outcomes(
                    &mut output,
                    &presented.intent_outcomes,
                );
                // The realized declarations are reported after the presented
                // value; a declared start's launch receipt is host-facing only.
                let mut model_return = presented.presentation.model_return;
                model_return.parts.extend(
                    super::super::pending_resolver::model_visible_outcomes(
                        &captured.intents,
                        &presented.intent_outcomes,
                    )
                    .iter()
                    .map(|outcome| crate::ModelToolReturnPart::text(outcome.model_addendum())),
                );
                (output, model_return, presented.intent_outcomes)
            }
            CallEnd::Withheld { decision, cause } => {
                let output = super::observations::terminal_output(decision, cause.as_ref(), None)
                    .map_err(encoding)?;
                let model_return =
                    crate::ModelToolReturn::from_output(prepared.call.tool_name.clone(), &output);
                (output, model_return, Vec::new())
            }
        };
        Ok(crate::sansio::CompletedToolCall {
            call_id: call_id.clone(),
            provider_call_id: prepared.call.provider_call_id,
            tool_name: prepared.call.tool_name,
            args: prepared.call.args,
            output,
            model_return,
            intent_outcomes,
            replay: prepared.call.replay,
        })
    }
}
