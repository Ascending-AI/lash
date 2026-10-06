use super::*;
use crate::plugin::{
    AfterToolDecision, BeforeToolDecision, PreparedCallReadView, ToolResultCandidate,
};

pub(super) fn context(
    dispatch: &ToolDispatchContext<'_>,
    prepared: &Prepared,
) -> crate::plugin::ToolHookContext {
    super::super::hooks::hook_context(
        dispatch,
        &prepared.call.call_id,
        &prepared.call.tool_id,
        &prepared.call.tool_name,
        prepared
            .input
            .definition
            .manifest
            .argument_projection
            .clone(),
    )
}

impl ProductionToolHandlers<'_> {
    pub(super) async fn check_before(
        &self,
        request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        let prepared: Prepared =
            serde_json::from_value(request.prepared.clone()).map_err(|error| error.to_string())?;
        let host = prepared.input.binding.preparation.clone();
        if let Some(failure) = &prepared.failure {
            return Ok(vec![AttributedVerdict {
                callback: host,
                verdict: BeforeCheckReply::Deny {
                    cause: cause("tool_failure", failure),
                },
            }]);
        }
        let dispatch = self.dispatch(&prepared.input).await?;
        let record = match dispatch
            .plugins
            .check_tool_args(
                &context(&dispatch, &prepared),
                &Arc::new(
                    prepared
                        .original_args
                        .clone()
                        .unwrap_or_else(|| prepared.call.args.clone()),
                ),
                &PreparedCallReadView::new(prepared.call.clone()),
            )
            .await
        {
            Ok(record) => record,
            Err(failure) => {
                return Ok(vec![AttributedVerdict {
                    callback: host,
                    verdict: BeforeCheckReply::Deny {
                        cause: cause("tool_failure", &failure),
                    },
                }]);
            }
        };
        let winner = record.winner().map(|reply| reply.callback.clone());
        let mut answers = Vec::new();
        for reply in record.replies().iter().cloned() {
            let verdict = match reply.verdict {
                BeforeToolDecision::Allow => BeforeCheckReply::Allow,
                BeforeToolDecision::Deny(failure) => BeforeCheckReply::Deny {
                    cause: cause("tool_failure", &failure),
                },
                BeforeToolDecision::Cancel(cancel) => BeforeCheckReply::Cancel {
                    cause: cause("tool_cancellation", &cancel),
                },
                BeforeToolDecision::AbortRun(abort) => BeforeCheckReply::AbortRun {
                    cause: cause("plugin_abort", &abort),
                },
                BeforeToolDecision::Cached(value) => {
                    let output = ToolCallOutput {
                        outcome: crate::ToolCallOutcome::Success(value.value),
                        control: None,
                        view: value.view,
                        projection_value: value.projection_value,
                    };
                    let captured = if winner.as_ref() == Some(&reply.callback) {
                        self.capture_output(
                            prepared.clone(),
                            output,
                            crate::plugin::ToolHookOccurrence::Cached,
                            ToolIntents::default(),
                            Vec::new(),
                            Vec::new(),
                        )
                        .await
                    } else {
                        Ok(Captured {
                            output,
                            messages: Vec::new(),
                            triggers: Vec::new(),
                            original: None,
                            occurrence: crate::plugin::ToolHookOccurrence::Cached,
                            intents: ToolIntents::default(),
                            start_refusal: None,
                        })
                    };
                    BeforeCheckReply::Cached {
                        output: encode(&captured?)?,
                    }
                }
            };
            answers.push(AttributedVerdict {
                callback: reply.callback,
                verdict,
            });
        }
        Ok(answers)
    }

    pub(super) async fn check_after(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        // An isolated call has no result candidate at D: its process has not
        // launched yet, so no after-check sees it.
        if matches!(
            capture,
            SingletonCapture::Isolated { .. }
                | SingletonCapture::Interrupted
                | SingletonCapture::TimedOut { evidence: None, .. }
                | SingletonCapture::Cancelled { evidence: None }
        ) {
            return Ok(Vec::new());
        }
        let captured: Captured = decode(capture.output().ok_or("final has no output")?)?;
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or("the final has no hydrated admission")?;
        if let crate::ToolCallOutcome::Cancelled(cancel) = &captured.output.outcome
            && cancel.origin == Some(crate::CancelOrigin::TurnStopped)
            && cancel.source == crate::ToolFailureSource::Cancellation
        {
            // This fact came from recorded X, so D carries the existing typed
            // cancellation verdict even when replay never reenters the body.
            return Ok(vec![AttributedVerdict {
                callback: prepared.input.binding.executable,
                verdict: AfterCheckVerdict::Cancel {
                    cause: cause("tool_cancellation", cancel),
                },
            }]);
        }
        let dispatch = self.dispatch(&prepared.input).await?;
        let original = captured.original.unwrap_or_else(|| captured.output.clone());
        let (original, _) = ToolResultCandidate::split(original);
        let (candidate, _) = ToolResultCandidate::split(captured.output);
        let checked = dispatch
            .plugins
            .check_tool_result(
                &context(&dispatch, &prepared),
                captured.occurrence,
                &PreparedCallReadView::new(prepared.call.clone()),
                &Arc::new(original),
                &Arc::new(candidate),
            )
            .await;
        let checks = match checked {
            Ok(checks) => checks,
            Err(failure) => {
                return Ok(vec![AttributedVerdict {
                    callback: prepared.input.binding.executable,
                    verdict: AfterCheckVerdict::Deny {
                        cause: cause("tool_failure", &failure),
                    },
                }]);
            }
        };
        self.contributions.lock_recover().insert(
            call_id.clone(),
            checks
                .contributions
                .into_iter()
                .map(|contribution| CheckContribution {
                    plugin_id: contribution.plugin_id,
                    messages: contribution.messages,
                    events: contribution.events,
                })
                .collect(),
        );
        if let Err(error) = crate::plugin::propose_all(&dispatch.plugins, checks.proposals) {
            return Ok(vec![AttributedVerdict {
                callback: prepared.input.binding.executable,
                verdict: AfterCheckVerdict::Deny {
                    cause: cause(
                        "tool_failure",
                        &crate::ToolFailure::runtime(
                            crate::ToolFailureClass::Internal,
                            "tool_state_unrecorded",
                            error.to_string(),
                        ),
                    ),
                },
            }]);
        }
        Ok(checks
            .record
            .replies()
            .iter()
            .cloned()
            .map(|reply| AttributedVerdict {
                callback: reply.callback,
                verdict: match reply.verdict {
                    AfterToolDecision::Allow => AfterCheckVerdict::Allow,
                    AfterToolDecision::Deny(failure) => AfterCheckVerdict::Deny {
                        cause: cause("tool_failure", &failure),
                    },
                    AfterToolDecision::Cancel(cancel) => AfterCheckVerdict::Cancel {
                        cause: cause("tool_cancellation", &cancel),
                    },
                    AfterToolDecision::AbortRun(abort) => AfterCheckVerdict::AbortRun {
                        cause: cause("plugin_abort", &abort),
                    },
                },
            })
            .collect())
    }

    pub(super) async fn present_capture(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        let fault = |message| SingletonPresentationError::Fault { message };
        let captured: Captured = match capture.output() {
            Some(output) => decode(output).map_err(fault)?,
            None => Captured {
                original: None,
                output: observations::terminal_output(
                    &CallDecision::Final {
                        source: ResultSource::Attempt {
                            attempt: AttemptOrdinal::FIRST,
                        },
                        declares: false,
                    },
                    None,
                    Some(capture),
                )
                .map_err(fault)?,
                messages: Vec::new(),
                triggers: Vec::new(),
                occurrence: crate::plugin::ToolHookOccurrence::Attempt {
                    attempt: AttemptOrdinal::FIRST,
                },
                intents: ToolIntents::default(),
                start_refusal: None,
            },
        };
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .ok_or_else(|| fault("the final has no hydrated admission".to_owned()))?;
        let dispatch = self.dispatch(&prepared.input).await.map_err(fault)?;
        let mut outcomes = self
            .declarations
            .lock_recover()
            .remove(call_id)
            .unwrap_or_default();
        outcomes.extend(captured.start_refusal.clone());
        let mut output = captured.output.clone();
        super::super::attempt_coordinator::project_recorded_intent_outcomes(&mut output, &outcomes);
        let facts = Arc::new(crate::plugin::ToolPresentationFacts {
            intent_outcomes: outcomes.clone(),
        });
        let projection = crate::plugin::ToolResultProjectionContext {
            owner: dispatch.owner.runtime_owner(),
            call_id: call_id.clone(),
            tool_id: prepared.call.tool_id,
            tool_name: prepared.call.tool_name.clone(),
            render: prepared.input.render.clone(),
            args: prepared.call.args.clone(),
            output,
            duration_ms: 0,
            artifacts: Arc::new(crate::runtime::effect::SessionPresentationArtifacts::new(
                self.context.attachment_store(),
            )),
        };
        let presentation = dispatch
            .plugins
            .present_tool_result(
                projection,
                facts,
                &prepared.input.binding.presentation,
                &dispatch.execution_env_spec.policy.attachment_acceptance,
            )
            .await
            .map_err(|error| {
                self.context.record_nested_effect_error(error.clone());
                fault(error.to_string())
            })?;
        encode(&Presented {
            presentation,
            intent_outcomes: outcomes,
        })
        .map_err(fault)
    }
}
