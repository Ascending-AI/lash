use super::RuntimeExecutionContext;
use lash_sansio::sync::MutexExt;

impl RuntimeExecutionContext<'_> {
    pub(in crate::session) async fn retain_unadmitted_tool_request(
        &self,
        record: &crate::ToolCallRecord,
        requested_at_ms: u64,
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Box::pin(async {
            let Some(store) = self
                .dispatch
                .effect_controller
                .frontier()
                .runtime()
                .and_then(|runtime| runtime.tool_receipts())
            else {
                return Ok(());
            };
            let owner =
                crate::EffectOpener::for_scope(&self.admitted_scope()).map_err(|error| {
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeToolRunShape,
                        error.to_string(),
                    )
                })?;
            let owner = crate::trace::run_receipts::tool_owner(&owner);
            let payload = serde_json::json!({
                "call_id": record.call_id,
                "provider_call_id": record.provider_call_id,
                "tool_name": record.tool,
                "args": record.args,
            });
            let digest = crate::stable_identity::rendered_hash(
                "unadmitted-tool-request",
                1,
                &crate::identity_json::payload_leaf(&payload),
            );
            let offered_digest = digest.clone();
            let request_key = format!("{}:{}", self.execution_scope_id(), record.call_id);
            let tracing = self.tracing.clone();
            let standing = tracing
                .as_ref()
                .map(|tracing| self.coordination_standing(tracing));
            let call = record.clone();
            let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
            let recorded = self
                .journaled_language_value_with(
                    format!("{request_key}:request"),
                    "retain-tool-request".into(),
                    move || async move {
                        let mut candidate = None;
                        let scope = tracing
                            .as_ref()
                            .and_then(|tracing| tracing.scope.as_ref())
                            .and_then(|parent| {
                                crate::trace::tool_trace_scope(
                                    parent,
                                    &call.call_id,
                                    requested_at_ms,
                                )
                            })
                            .map(|mut scope| {
                                if let Some(tracing) = &tracing {
                                    let proposed = tracing
                                        .runtime
                                        .scopes()
                                        .propose(&scope.scope, &scope.cause);
                                    scope.anchor = proposed.anchor();
                                    candidate = Some(proposed);
                                }
                                scope
                            });
                        let request = crate::store::ToolRequestReceipt {
                            owner,
                            request_key,
                            payload_digest: digest,
                            payload,
                            scope,
                            context: tracing
                                .as_ref()
                                .map(|tracing| tracing.scope_context.clone())
                                .unwrap_or_default(),
                            requested_at_ms,
                        };
                        let receipt = store.record_tool_request(&request).await;
                        if let Some(candidate) = candidate {
                            candidate.settle(match &receipt {
                                Ok(receipt) if receipt.changed => {
                                    lash_trace::TraceCandidateOutcome::Selected
                                }
                                Ok(_) => lash_trace::TraceCandidateOutcome::Reused,
                                Err(_) => lash_trace::TraceCandidateOutcome::Refused,
                            });
                        }
                        let receipt = receipt.map_err(crate::RuntimeEffectControllerError::from)?;
                        if let (Some(standing), Some(scope)) = (&standing, &receipt.record.scope) {
                            standing.under(scope.clone()).transition(
                                receipt.permit().as_ref(),
                                receipt.record.requested_at_ms,
                                lash_trace::TraceTransitionKind::Started,
                                0,
                                || {
                                    (
                                        receipt.record.context.clone(),
                                        lash_trace::TraceEvent::ToolCallStarted {
                                            call_id: call.call_id,
                                            provider_call_id: call.provider_call_id,
                                            name: call.tool,
                                            args: call.args,
                                            issuing_node_id: issuing_node,
                                        },
                                    )
                                },
                            );
                        }
                        serde_json::to_value(receipt.record).map_err(|error| {
                            crate::RuntimeEffectControllerError::new(
                                crate::RuntimeErrorCode::RuntimeToolRunShape,
                                error.to_string(),
                            )
                        })
                    },
                )
                .await?;
            let request: crate::store::ToolRequestReceipt = serde_json::from_value(recorded)
                .map_err(|error| {
                    crate::RuntimeEffectControllerError::new(
                        crate::RuntimeErrorCode::RuntimeToolRunShape,
                        error.to_string(),
                    )
                })?;
            if request.payload_digest != offered_digest {
                return Err(crate::RuntimeEffectControllerError::from(
                    crate::store::StoreError::ToolRequestConflict {
                        owner: request.owner,
                        request_key: request.request_key,
                    },
                ));
            }
            self.tool_requests
                .lock_recover()
                .insert(record.call_id.clone(), request);
            Ok(())
        })
        .await
    }

    pub(in crate::session) async fn emit_tool_call_completed_trace(
        &self,
        record: &crate::ToolCallRecord,
        attempts: &[lash_trace::TraceRetryAttempt],
        intent_outcomes: &[crate::ToolIntentExecutionOutcome],
    ) -> Result<(), crate::RuntimeEffectControllerError> {
        Box::pin(async {
            let Some(store) = self
                .dispatch
                .effect_controller
                .frontier()
                .runtime()
                .and_then(|runtime| runtime.tool_receipts())
            else {
                return Ok(());
            };
            let request = self
                .tool_requests
                .lock_recover()
                .get(&record.call_id)
                .cloned();
            let Some(request) = request else {
                return Ok(());
            };
            let record = record.clone();
            let attempts = attempts.to_vec();
            let intent_outcomes = serde_json::to_value(intent_outcomes).map_err(|error| {
                crate::RuntimeEffectControllerError::new(
                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                    error.to_string(),
                )
            })?;
            let tracing = self.tracing.clone();
            let standing = tracing
                .as_ref()
                .map(|tracing| self.coordination_standing(tracing));
            let clock = self.dispatch.clock.clone();
            let issuing_node = self.issuing_language_node_id.as_deref().map(str::to_string);
            self.journaled_language_value_with(
                format!("{}:completion", request.request_key),
                "retain-tool-completion".into(),
                move || async move {
                    let receipt = store
                        .record_tool_completion(&crate::store::ToolCompletionReceipt {
                            owner: request.owner.clone(),
                            request_key: request.request_key.clone(),
                            payload_digest: request.payload_digest.clone(),
                            result: serde_json::to_value(&record).map_err(|e| {
                                crate::RuntimeEffectControllerError::new(
                                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                                    e.to_string(),
                                )
                            })?,
                            intent_outcomes,
                            completed_at_ms: clock.timestamp_ms(),
                        })
                        .await
                        .map_err(crate::RuntimeEffectControllerError::from)?;
                    if let Some(tracing) = &tracing {
                        let outcomes: Vec<crate::ToolIntentExecutionOutcome> =
                            serde_json::from_value(receipt.record.intent_outcomes.clone())
                                .map_err(|error| {
                                    crate::RuntimeEffectControllerError::new(
                                        crate::RuntimeErrorCode::RuntimeToolRunShape,
                                        error.to_string(),
                                    )
                                })?;
                        for outcome in outcomes {
                            let Some(kind) = outcome.kind() else {
                                continue;
                            };
                            match outcome {
                                crate::ToolIntentExecutionOutcome::Executed { .. } => {
                                    crate::operational_metrics::record_tool_intent_executed(
                                        tracing.runtime.metrics(),
                                        receipt.permit().as_ref(),
                                        kind.as_str(),
                                    )
                                }
                                crate::ToolIntentExecutionOutcome::Refused { refusal, .. } => {
                                    crate::operational_metrics::record_tool_intent_refused(
                                        tracing.runtime.metrics(),
                                        receipt.permit().as_ref(),
                                        kind.as_str(),
                                        refusal.code().as_ref(),
                                    )
                                }
                                crate::ToolIntentExecutionOutcome::ProtocolRefused { .. } => {}
                            }
                        }
                    }
                    if let (Some(standing), Some(scope)) = (&standing, &request.scope) {
                        let stored: crate::ToolCallRecord =
                            serde_json::from_value(receipt.record.result.clone()).map_err(|e| {
                                crate::RuntimeEffectControllerError::new(
                                    crate::RuntimeErrorCode::RuntimeToolRunShape,
                                    e.to_string(),
                                )
                            })?;
                        standing.under(scope.clone()).transition(
                            receipt.permit().as_ref(),
                            receipt.record.completed_at_ms,
                            lash_trace::TraceTransitionKind::Terminal,
                            0,
                            || {
                                (
                                    request.context.clone(),
                                    lash_trace::TraceEvent::ToolCallCompleted {
                                        call_id: stored.call_id.clone(),
                                        provider_call_id: stored.provider_call_id.clone(),
                                        name: stored.tool.clone(),
                                        args: stored.args.clone(),
                                        output: crate::trace::trace_tool_call_output(
                                            &stored.output,
                                        ),
                                        duration_ms: receipt
                                            .record
                                            .completed_at_ms
                                            .saturating_sub(request.requested_at_ms),
                                        issuing_node_id: issuing_node.clone(),
                                        attempts: (!attempts.is_empty()).then_some(attempts),
                                    },
                                )
                            },
                        );
                    }
                    Ok(serde_json::Value::Null)
                },
            )
            .await?;
            Ok(())
        })
        .await
    }
}
