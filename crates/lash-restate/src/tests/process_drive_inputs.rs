//! The inputs a process segment's drive decides by are recorded (FIG-3673):
//! a redrive under a changed host reads what the first execution read.

use super::*;

/// Reports, as its terminal value, whether its segment's controller would cut
/// a boundary after two effects: the boundary policy the segment runs under.
#[derive(Debug)]
struct BoundaryPolicyProbeRunner;

#[async_trait::async_trait]
impl RestateProcessRunner for BoundaryPolicyProbeRunner {
    fn executable_generation(
        &self,
        _registration: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _started: &SegmentStarted,
        _process_id: ProcessId,
        _registration: ProcessRegistration,
        _execution_context: ProcessExecutionContext,
        scoped_effect_controller: ScopedEffectController<'_>,
        _handover: Option<lash_core::SegmentHandover>,
        _cancellation: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        let cuts_after_two = scoped_effect_controller
            .controller()
            .wants_segment_boundary(&lash_core::SegmentProgress {
                effects_executed: 2,
                ..lash_core::SegmentProgress::default()
            })
            .is_some();
        Ok(process_success(serde_json::json!({ "cuts_after_two": cuts_after_two })).into())
    }
}

/// A segment's boundary policy is an input its admission records (FIG-3673):
/// a redrive under a host whose selector changed since the segment was
/// admitted cuts where the recorded policy says, so a replayed segment never
/// moves its cut.
#[tokio::test]
pub(super) async fn a_boundary_policy_change_between_attempts_replays_the_recorded_cut() {
    let registry = process_registry();
    let registration = rerunnable_registration();
    let process_id = registry
        .register_process(registration.clone())
        .await
        .expect("register the policy probe process")
        .id;
    let endpoint_with_budget = |budget: u64| {
        Endpoint::builder()
            .bind(
                LashProcessWorkflowImpl::new_for_test(
                    Arc::new(BoundaryPolicyProbeRunner),
                    Arc::clone(&registry),
                    continuation_store(),
                )
                .with_segment_effect_budget_selector(move |_| budget)
                .serve(),
            )
            .build()
    };
    let input = RestateProcessWorkflowInput {
        process_id: process_id.clone(),
        registration,
        execution_context: ProcessExecutionContext::default(),
        segment_ordinal: 0,
        journal_version: RESTATE_PROCESS_JOURNAL_VERSION,
    };

    // Admitted under a host that cuts every two effects.
    let admitting = endpoint_with_budget(2);
    let admission = admission_journal(&admitting, process_id.as_str(), &input)
        .await
        .expect("the first attempt admits its segment");

    // Redriven under a host whose selector now says seven.
    let redriving = endpoint_with_budget(7);
    let output = invoke_endpoint_body_with_json_call_responses(
        &redriving,
        "LashProcessWorkflow",
        "run",
        admitted_invocation_body(process_id.as_str(), &input, &admission)
            .expect("splice the admission"),
        Vec::new(),
    )
    .await
    .expect("the redrive runs its segment");
    let Some(RestateProcessWorkflowOutput::Terminal { output }) =
        restate_output_json::<RestateProcessWorkflowOutput>(&output)
    else {
        panic!(
            "the probe reaches its terminal: {:?}",
            restate_error_message(&output)
        );
    };
    assert_eq!(
        *output,
        process_success(serde_json::json!({ "cuts_after_two": true })),
        "the redrive cuts under the policy its admission recorded, not the live selector"
    );
}
