//! Recorded semantic channels applied after protected V acceptance.
use super::*;

impl ProductionToolHandlers<'_> {
    pub(super) fn incorporate_capture(
        &self,
        call_id: &crate::ToolCallId,
        capture: Option<&SingletonCapture>,
        presentation: Option<&str>,
        observe: bool,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        let fault = |message| {
            crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::RecordEncodingFailed,
                message,
            )
        };
        let captured: Option<Captured> = capture
            .and_then(SingletonCapture::output)
            .map(decode)
            .transpose()
            .map_err(fault)?;
        let presented: Option<Presented> = presentation.map(decode).transpose().map_err(fault)?;
        self.prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or_else(|| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::EffectReplayDivergence,
                    "the incorporated call has no admission",
                )
            })?;
        let contributions = self
            .contributions
            .lock_recover()
            .get(call_id)
            .cloned()
            .unwrap_or_default();
        let outcomes = presented
            .as_ref()
            .map(|presented| presented.intent_outcomes.clone())
            .unwrap_or_default();
        let possession = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                crate::ToolIntentExecutionOutcome::Executed {
                    realized: crate::ToolIntentRealized::StartProcess(handle),
                    ..
                } => Some(handle.process_id.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        let messages = captured
            .as_ref()
            .into_iter()
            .flat_map(|captured| captured.messages.clone())
            .chain(
                contributions
                    .iter()
                    .flat_map(|contribution| contribution.messages.clone()),
            )
            .collect::<Vec<_>>();
        let triggers = captured
            .as_ref()
            .map(|capture| capture.triggers.clone())
            .unwrap_or_default();
        self.context.incorporate_tool_facts(
            crate::session::SettlementSource::Invocation {
                call_id: call_id.clone(),
            },
            &possession,
            &messages,
            &triggers,
        )?;
        if observe {
            let mut cursor = self
                .context
                .dispatch()
                .observation_cursor(&format!("run:{call_id}:after"));
            for contribution in contributions {
                crate::plugin::observe_plugin_runtime_events(
                    &mut cursor,
                    self.context.dispatch().observer.as_ref(),
                    &contribution.plugin_id,
                    contribution.events,
                );
            }
        }
        Ok(())
    }
}
