//! One admission contract for every effect host (FIG-4225).
//!
//! Tool-intent admission is owned by the engine: its effect journal replays a
//! repeated identity, and the durable key each shape lands on fences a
//! redelivery the journal cannot see. The facade adds only two things beside
//! it: it binds a cancel identity to the target it first named, and it
//! retains every outcome in the durable submission ledger. A host that
//! journals nothing (the recording conformance host) runs the same contract,
//! so the laws below hold on it exactly as on a journaling host.

use super::*;

/// The recording conformance host: it journals no effect.
async fn recording_ingress_core() -> Result<(LashCore, Arc<dyn ProcessRegistry>, ProcessId)> {
    ingress_core_over(memory_store_backend().await, None, None).await
}

fn ingress_of(core: &LashCore) -> Result<crate::tools::ToolIntentIngress> {
    core.tool_intents(SESSION, lash_core::ExecutionScope::turn(SESSION, SCOPE))
}

/// The outcome the durable submission ledger retains for `identity`, read by
/// claiming the identity again: an existing row answers what it holds.
async fn ledger_outcome(
    registry: &Arc<dyn ProcessRegistry>,
    identity: &lash_core::ToolIntentIdentity,
    intent: lash_core::ToolIntent,
) -> Result<Option<lash_core::ToolIntentExecutionOutcome>> {
    let record = lash_core::ToolIntentSubmissionRecord::new(identity.clone(), intent)
        .expect("hash the submission");
    match lash_core::ProcessToolIntents::admit_tool_intent_submission(registry.as_ref(), record)
        .await?
    {
        lash_core::ToolIntentSubmissionAdmission::Existing(existing) => Ok(existing.outcome),
        lash_core::ToolIntentSubmissionAdmission::Admitted => {
            panic!("the submission must already hold a ledger row")
        }
    }
}

fn executed_outcome(
    outcome: &crate::tools::ToolIntentIngressOutcome,
) -> lash_core::ToolIntentExecutionOutcome {
    match outcome {
        crate::tools::ToolIntentIngressOutcome::Admitted {
            outcome: executed @ lash_core::ToolIntentExecutionOutcome::Executed { .. },
            ..
        } => executed.clone(),
        other => panic!("the submission must execute, got {other:?}"),
    }
}

async fn assert_ledger_retains_the_outcome(
    core: &LashCore,
    registry: &Arc<dyn ProcessRegistry>,
    process: &ProcessId,
) -> Result<()> {
    let ingress = ingress_of(core)?;
    let key = ingress
        .key("retained-outcome", 0)
        .expect("a host submission handle");
    let intent = emit_intent(&SessionId::from(SESSION), process);
    let submitted = ingress.submit(key.clone(), intent.clone()).await;
    let executed = executed_outcome(&submitted);
    assert_eq!(
        ledger_outcome(registry, key.identity(), intent).await?,
        Some(executed),
        "the ledger retains the outcome the submission answered"
    );
    Ok(())
}

#[tokio::test]
async fn a_journaling_host_retains_every_outcome_in_the_submission_ledger() -> Result<()> {
    let (core, registry, process) = ingress_core(memory_store_backend().await).await?;
    assert_ledger_retains_the_outcome(&core, &registry, &process).await
}

#[tokio::test]
async fn a_host_that_journals_nothing_retains_every_outcome_in_the_submission_ledger() -> Result<()>
{
    let (core, registry, process) = recording_ingress_core().await?;
    assert_ledger_retains_the_outcome(&core, &registry, &process).await
}

#[tokio::test]
async fn a_host_that_journals_nothing_binds_a_cancel_identity_to_its_first_target() -> Result<()> {
    let (core, registry, process) = recording_ingress_core().await?;
    let other = registry
        .register_process_with_observers(
            lash_core::ProcessRegistration::new(
                lash_core::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash_core::ProcessProvenance::host(),
                lash_core::Lifetime::Detached,
            ),
            &[SessionId::from(SESSION)],
        )
        .await?;
    let ingress = ingress_of(&core)?;
    let key = ingress
        .key("bound-cancel", 0)
        .expect("a host submission handle");

    let first = ingress
        .submit(
            key.clone(),
            cancel_intent(&SessionId::from(SESSION), &process),
        )
        .await;
    executed_outcome(&first);
    let changed = ingress
        .submit(
            key,
            cancel_intent_for_target(&SessionId::from(SESSION), &other.id),
        )
        .await;
    assert_eq!(
        changed,
        crate::tools::ToolIntentIngressOutcome::Refused {
            refusal: crate::tools::ToolIntentIngressRefusal::DuplicateIdentity {
                kind: lash_core::ToolIntentKind::CancelProcess,
            },
        },
        "a cancel identity is bound to the target it first named"
    );
    assert!(
        registry
            .get_process(&other.id)
            .await?
            .expect("the second target exists")
            .cancel_request
            .is_none(),
        "the refused change never reaches the second target"
    );
    Ok(())
}

/// A host that journals nothing is not a second admission owner: a
/// re-submitted identity meets the durable key its first submission landed
/// on and answers that first outcome, replayed, exactly as a redelivery onto
/// a journaling host's empty journal does. It is not refused from the ledger.
#[tokio::test]
async fn a_host_that_journals_nothing_answers_a_resubmitted_identity_with_its_first_outcome()
-> Result<()> {
    let (core, registry, _process) = recording_ingress_core().await?;
    let ingress = ingress_of(&core)?;
    let key = ingress
        .key("resubmitted-start", 0)
        .expect("a host submission handle");

    let first = ingress
        .submit(key.clone(), start_intent(&SessionId::from(SESSION)))
        .await;
    let resubmitted = ingress
        .submit(key, start_intent(&SessionId::from(SESSION)))
        .await;

    assert!(
        matches!(
            &first,
            crate::tools::ToolIntentIngressOutcome::Admitted {
                replayed: false,
                ..
            }
        ),
        "the first submission realizes the start: {first:?}"
    );
    assert!(
        matches!(
            &resubmitted,
            crate::tools::ToolIntentIngressOutcome::Admitted { replayed: true, .. }
        ),
        "the resubmission replays the first outcome: {resubmitted:?}"
    );
    assert_eq!(
        started_process_id(&resubmitted),
        started_process_id(&first),
        "the resubmission names the process the first submission started"
    );
    assert_eq!(
        registered_process_count(&registry).await?,
        2,
        "the fixture process and the one start"
    );
    Ok(())
}
