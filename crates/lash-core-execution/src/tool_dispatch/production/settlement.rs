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
        // An isolated final's presentation is the descriptor of the process
        // it started, which the session now possesses.
        let isolated = matches!(capture, Some(SingletonCapture::Isolated { .. }));
        let started: Option<super::IsolatedProcessDescriptor> = presentation
            .filter(|_| isolated)
            .map(decode)
            .transpose()
            .map_err(fault)?;
        let presented: Option<Presented> = presentation
            .filter(|_| !isolated)
            .map(decode)
            .transpose()
            .map_err(fault)?;
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
        // A presented call reports its recorded outcomes. A call that ends
        // unpresented after its declared start launched still possesses the
        // child its launch receipt names.
        let outcomes = match (&presented, presentation) {
            (Some(presented), _) => presented.intent_outcomes.clone(),
            (None, None) => self
                .declarations
                .lock_recover()
                .remove(call_id)
                .unwrap_or_default(),
            (None, Some(_)) => Vec::new(),
        };
        let possession = outcomes
            .iter()
            .filter_map(|outcome| match outcome {
                crate::ToolIntentExecutionOutcome::Executed {
                    realized: crate::ToolIntentRealized::StartProcess(handle),
                    ..
                } => Some(handle.process_id.clone()),
                _ => None,
            })
            .chain(started.map(|descriptor| descriptor.process_id))
            .collect::<Vec<_>>();
        self.context.incorporate_tool_facts(
            crate::session::SettlementSource::Invocation {
                call_id: call_id.clone(),
            },
            &possession,
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
