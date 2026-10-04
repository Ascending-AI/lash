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
        call: &SingletonToolCall,
        request: &SingletonPreparedRequest,
    ) -> Vec<AttributedVerdict<BeforeCheckReply>> {
        let prepared: Prepared = match serde_json::from_value(request.prepared.clone()) {
            Ok(prepared) => prepared,
            Err(error) => {
                return vec![AttributedVerdict {
                    callback: call.binding.preparation.clone(),
                    verdict: BeforeCheckReply::Deny {
                        cause: cause(
                            "tool_failure",
                            &crate::ToolFailure::runtime(
                                crate::ToolFailureClass::Internal,
                                "prepared_request_unreadable",
                                error.to_string(),
                            ),
                        ),
                    },
                }];
            }
        };
        let host = prepared.input.binding.preparation.clone();
        if let Some(failure) = &prepared.failure {
            return vec![AttributedVerdict {
                callback: host,
                verdict: BeforeCheckReply::Deny {
                    cause: cause("tool_failure", failure),
                },
            }];
        }
        let dispatch = self.dispatch(&prepared.input);
        let record = match dispatch
            .plugins
            .check_tool_args(
                &context(&dispatch, &prepared),
                &Arc::new(prepared.original_args.clone()),
                &PreparedCallReadView::new(prepared.call.clone()),
            )
            .await
        {
            Ok(record) => record,
            Err(failure) => {
                return vec![AttributedVerdict {
                    callback: host,
                    verdict: BeforeCheckReply::Deny {
                        cause: cause("tool_failure", &failure),
                    },
                }];
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
                        })
                    };
                    match captured.and_then(|capture| encode(&capture)) {
                        Ok(output) => BeforeCheckReply::Cached { output },
                        Err(message) => BeforeCheckReply::Deny {
                            cause: cause(
                                "tool_failure",
                                &crate::ToolFailure::runtime(
                                    crate::ToolFailureClass::Internal,
                                    "cached_result_failed",
                                    message,
                                ),
                            ),
                        },
                    }
                }
            };
            answers.push(AttributedVerdict {
                callback: reply.callback,
                verdict,
            });
        }
        answers
    }

    pub(super) async fn check_after(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Vec<AttributedVerdict<AfterCheckVerdict>> {
        let Some(output) = capture.output() else {
            return Vec::new();
        };
        let captured: Captured = match decode(output) {
            Ok(captured) => captured,
            Err(_) => return Vec::new(),
        };
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .expect("K3 hydrated the final's admission");
        let dispatch = self.dispatch(&prepared.input);
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
                return vec![AttributedVerdict {
                    callback: prepared.input.binding.executable,
                    verdict: AfterCheckVerdict::Deny {
                        cause: cause("tool_failure", &failure),
                    },
                }];
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
            return vec![AttributedVerdict {
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
            }];
        }
        checks
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
            .collect()
    }

    pub(super) async fn present_capture(
        &self,
        call_id: &crate::ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, SingletonPresentationError> {
        let fault = |message| SingletonPresentationError::Fault { message };
        let captured: Captured = decode(
            capture
                .output()
                .ok_or_else(|| fault("final has no output".to_owned()))?,
        )
        .map_err(fault)?;
        let prepared = self
            .prepared
            .lock_recover()
            .get(call_id)
            .cloned()
            .expect("K3 hydrated the final's admission");
        let dispatch = self.dispatch(&prepared.input);
        let outcomes = self
            .declarations
            .lock_recover()
            .remove(call_id)
            .unwrap_or_default();
        let output = captured.output.clone();
        let settlement = Arc::new(crate::runtime::effect::ToolSettlement {
            version: crate::runtime::effect::TOOL_SETTLEMENT_VERSION,
            intent_outcomes: outcomes.clone(),
            possession: Vec::new(),
            triggers: Vec::new(),
            checkpoint_messages: Vec::new(),
            stream: Default::default(),
            model_return: crate::ModelToolReturn::from_output(
                prepared.call.tool_name.clone(),
                &output,
            ),
        });
        let projection = crate::plugin::ToolResultProjectionContext {
            owner: dispatch.owner.runtime_owner(),
            call_id: call_id.clone(),
            tool_id: prepared.call.tool_id,
            tool_name: prepared.call.tool_name.clone(),
            render: None,
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
                settlement,
                &prepared.input.binding.presentation,
                self.context.attachment_acceptance(),
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
