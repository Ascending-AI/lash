//! The ordinary step host used by process lifecycle laws.
use super::*;

#[async_trait::async_trait]
impl ProcessSteps for LawSteps {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    fn assess(
        &self,
        _runtime: &Arc<crate::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _target: Option<&crate::runtime::process::steps::InvocationTarget>,
    ) -> Result<crate::runtime::process::steps::StepAssessment, StepRefusal> {
        let policy = match step_tool(step) {
            LAW_FLAKY => ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(3).unwrap_or(std::num::NonZeroU32::MIN),
                10,
                50,
            ),
            _ => ExecutionPolicy::Once,
        };

        Ok(crate::runtime::process::steps::StepAssessment::Current {
            tool: lash_vocabulary::tool_contract::ToolId::new(step_tool(step)),
            policy,
        })
    }

    async fn admit(
        &self,
        _runtime: &Arc<crate::runtime::process::StepRuntime>,
        _process: &ProcessRecord,
        step: &StepRequest,
        _target: Option<&crate::runtime::process::steps::InvocationTarget>,
        now_ms: u64,
    ) -> Result<StepAdmission, StepRefusal> {
        let policy = match step_tool(step) {
            LAW_FLAKY => ExecutionPolicy::repeatable(
                std::num::NonZeroU32::new(3).unwrap_or(std::num::NonZeroU32::MIN),
                10,
                50,
            ),
            _ => ExecutionPolicy::Once,
        };
        let limit = ExecutionLimit::starting_at(now_ms, LONG, LONG);
        Ok(StepAdmission::new(
            lash_vocabulary::tool_contract::ToolId::new(step_tool(step)),
            policy,
            limit,
            (step_tool(step) == LAW_PARK).then(|| {
                ParkDeadline::At(WaitDeadline::at_instant(lash_durable::DurableInstant(
                    i64::try_from(limit.expires_at).unwrap_or(i64::MAX),
                )))
            }),
        ))
    }

    fn resolved(
        &self,
        process: &ProcessRecord,
        _step: &StepRequest,
        _execution: &AdmittedExecution,
        parked: &Material<CompletionSource>,
        resolution: Resolution,
    ) -> SettledOutput {
        let text = match resolution {
            Resolution::Ok(value) => {
                json!({ "resolved": value, "parked": parked.payload() }).to_string()
            }
            other => format!("{other:?}"),
        };
        SettledOutput::Completed(step_output(&process.id, &text))
    }

    fn body(
        &self,
        _runtime: &std::sync::Arc<crate::runtime::process::StepRuntime>,
        process: &ProcessRecord,
        step: &StepRequest,
        execution: &AdmittedExecution,
    ) -> crate::runtime::actor::round::MemberBody {
        if step_tool(step) == LAW_PAGED {
            return passes::paged_body(process.id.clone(), execution.call().clone());
        }
        crate::runtime::actor::round::member_body({
            let tool = step_tool(step).to_owned();
            let owner = process.id.clone();
            let tag = match process.input.as_ref() {
                ProcessInput::Engine { payload, .. } => {
                    payload["tag"].as_str().unwrap_or_default().to_owned()
                }
                _ => String::new(),
            };
            let call = execution.call().clone();
            let attempt = execution.attempt();
            // A step that may park re-derives the key of the wait its admission
            // pinned, as a round member does.
            let key = execution
                .draft()
                .pinned_wait()
                .and_then(|pinned| waits::host_key(&pinned.wait()).map(|key| (pinned.id, key)));
            Box::new(move |_token| {
                Box::pin(async move {
                    *STEP_ENTRIES
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .entry(call.as_str().to_owned())
                        .or_default() += 1;
                    match tool.as_str() {
                        LAW_FLAKY if attempt == 1 => SettledOutput::Failed(
                            step_output(&owner, "failed once")
                                .failure(KnownFailureReason::Reported, Some(10)),
                        ),
                        LAW_FLAKY => SettledOutput::Completed(step_output(
                            &owner,
                            &format!("succeeded on attempt {attempt}"),
                        )),
                        LAW_READY => {
                            let gate = READY_GATES
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .get(&tag)
                                .cloned();
                            if let Some(gate) = gate {
                                gate.wait().await;
                            }
                            SettledOutput::Completed(step_output(&owner, "ready"))
                        }
                        LAW_PARK => {
                            let Some((wait, key)) = key else {
                                return SettledOutput::Interrupted;
                            };
                            PARKED_KEYS
                                .lock()
                                .unwrap_or_else(PoisonError::into_inner)
                                .insert(tag, key.as_str().to_owned());
                            SettledOutput::Waiting(
                                step_output(&owner, "parked").parked(wait.to_hex()),
                            )
                        }
                        _ => std::future::pending().await,
                    }
                })
            })
        })
    }
}
