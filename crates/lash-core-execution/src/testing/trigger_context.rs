use super::{TestExecutionContextBuilder, TestExecutionPorts, process_work_wiring_for_registry};
use std::sync::Arc;

/// Build an empty code-execution context whose trigger router delivers through
/// `trigger_store` to processes in `process_registry`.
pub fn code_execution_context_with_trigger_store<'run>(
    ports: impl Into<TestExecutionPorts>,
    trigger_store: Arc<dyn crate::TriggerStore>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .trigger_router(Some(test_trigger_router(trigger_store, process_registry)))
        .build()
        .into_runtime()
}

/// [`code_execution_context_with_trigger_store`] under a stable parent
/// invocation.
pub fn code_execution_context_with_trigger_store_and_invocation<'run>(
    ports: impl Into<TestExecutionPorts>,
    trigger_store: Arc<dyn crate::TriggerStore>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
    invocation: crate::RuntimeInvocation,
) -> crate::RuntimeExecutionContext<'run> {
    TestExecutionContextBuilder::new(ports.into())
        .trigger_router(Some(test_trigger_router(trigger_store, process_registry)))
        .runtime_parent_invocation(invocation)
        .build()
        .into_runtime()
}

/// A trigger router over `trigger_store`, for fixtures building their own context.
pub fn test_trigger_router(
    trigger_store: Arc<dyn crate::TriggerStore>,
    process_registry: Arc<dyn crate::ProcessRegistry>,
) -> crate::TriggerRouter {
    crate::TriggerRouter::new(
        trigger_store,
        process_work_wiring_for_registry(process_registry),
    )
}

/// Record `request`'s occurrence through the start a trigger router commits:
/// plan it, prepare a fixture process for each delivery its plan matched, and
/// commit the occurrence, the processes and the deliveries bound to them in
/// one `trigger.start` mailbox transaction. An occurrence already recorded
/// answers its recorded deliveries, written by nobody.
///
/// # Errors
///
/// The plan's or the commit's refusal: an identity conflict, a reclaimed
/// occurrence, a store failure.
pub async fn record_trigger_occurrence(
    triggers: &dyn crate::TriggerStore,
    registry: &dyn crate::ProcessRegistry,
    durable: &dyn lash_durable::DurableStore,
    request: crate::TriggerOccurrenceRequest,
) -> Result<crate::TriggerIngressReceipt, crate::PluginError> {
    const ATTEMPTS: usize = 8;
    for _ in 0..ATTEMPTS {
        let (occurrence, mut subscriptions) = match triggers.plan_occurrence(&request).await? {
            crate::TriggerOccurrencePlan::Held(receipt) => return Ok(receipt),
            crate::TriggerOccurrencePlan::Fresh {
                occurrence,
                subscriptions,
            } => (occurrence, subscriptions),
        };
        crate::triggers::sort_trigger_subscriptions(&mut subscriptions);
        let mut deliveries = Vec::with_capacity(subscriptions.len());
        for subscription in &subscriptions {
            let registration = crate::runtime::accepted_process_registration()
                .with_start_key(Some(crate::triggers::delivery_start_key(
                    &occurrence,
                    subscription,
                )))
                .with_execution_env_ref(Some(subscription.env_ref.clone()));
            let prepared = registry
                .prepare_process_registration(registration, &[])
                .await?;
            let anchor = prepared.trace().anchor().clone();
            let (registration, observers, process_id, _, _) = prepared.into_commit(anchor);
            deliveries.push(crate::triggers::TriggerDeliveryStartRows::Started {
                subscription: subscription.clone(),
                registration: Box::new(registration),
                observers,
                process_id,
            });
        }
        let rows = crate::triggers::TriggerStartRows {
            planned: subscriptions
                .iter()
                .map(crate::triggers::TriggerSubscriptionFence::of)
                .collect(),
            occurrence,
            deliveries,
        };
        let committed = durable
            .commit_mail(rows.mail_tx()?, lash_durable::CommitLabel::TRIGGER_START)
            .await;
        let Some(_) = rows.answer(committed)? else {
            continue;
        };
        let mut reservations = rows.reservations();
        let occurrence = rows.occurrence;
        crate::facade_support::sort_trigger_delivery_reservations(&mut reservations);
        return Ok(crate::TriggerIngressReceipt {
            occurrence,
            reservations,
            realization: crate::StoreRealization::from_wrote(true),
        });
    }
    Err(crate::PluginError::StoreUnavailable {
        fault: crate::store::StoreFault::Contended,
    })
}
