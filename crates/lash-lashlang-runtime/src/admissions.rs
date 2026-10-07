//! What admitting a run's operations records (ADR 0132 §8).

use lash_vm_broker::{Admission, OperationRequest, OperationRequestCodec as _};

use crate::OperationAdmissions;

/// The admissions of one run's operations. A resource operation, alone or
/// in a batch, is one execution: its tool named by its receiver and
/// operation, its policy what the host declares for it (`Once` when it
/// declares none) and its limit the run's. A wait (an await, a sleep, a
/// signal wait) is admitted as no execution: a restore performs it again.
/// A sleep pins its timer with its admission, due at the absolute deadline
/// it was admitted with, so a restore races that timer and never sleeps
/// its whole duration again (ADR 0132 §6).
pub struct RunAdmissions<'a> {
    /// The actor whose store clock dates a sleep's deadline, and whose
    /// execution scope revokes its timer.
    pub cx: &'a lash_core::ActorContext,
    /// The run that owns every operation's material.
    pub opener: lash_core::EffectOpener,
    /// The limit every execution is admitted under.
    pub limit: lash_sansio::ExecutionLimit,
    /// The policy the host declares for a receiver's operation.
    pub policy: &'a (dyn Fn(&str, &str) -> Option<lash_sansio::ExecutionPolicy> + Send + Sync),
    /// The host's own state at a quiet point (a cell's envelope), committed
    /// with the VM's snapshot.
    pub host_state:
        &'a (dyn Fn() -> Result<Option<lash_vm_protocol::EncodedPayload>, String> + Send + Sync),
}

#[async_trait::async_trait]
impl OperationAdmissions for RunAdmissions<'_> {
    async fn admission(
        &self,
        call: &lash_sansio::ToolCallId,
        request: &OperationRequest,
    ) -> Result<Admission, String> {
        if let OperationRequest::Sleep(sleep) = request {
            // A sleep its host refuses (its value is no duration or
            // deadline) pins nothing: the host answers the guest the
            // refusal.
            let Ok(spec) = crate::bridge::process_sleep(sleep.kind, &sleep.value) else {
                return Ok(Admission::default());
            };
            let timer = lash_core::waits::timer(self.cx, spec)
                .await
                .map_err(|error| error.to_string())?;
            return Ok(Admission {
                draft: None,
                waits: vec![timer],
            });
        }
        if lash_vm_broker::waits_only(request) {
            return Ok(Admission::default());
        }
        let (tool, policy) = match request {
            OperationRequest::ResourceOperation(op) => {
                let receiver = match &op.receiver {
                    lashlang::Value::Resource(receiver) => receiver.alias.clone(),
                    _ => "value".to_owned(),
                };
                let policy = (self.policy)(&receiver, &op.operation);
                (format!("{receiver}.{}", op.operation), policy)
            }
            _ => ("batch".to_owned(), None),
        };
        let draft = lash_vm_broker::operation_draft(
            call.clone(),
            lash_sansio::ToolId::new(tool),
            &request.encode(),
            &self.opener,
            policy.unwrap_or(lash_sansio::ExecutionPolicy::Once),
            self.limit,
        )
        .map_err(|fault| fault.to_string())?;
        Ok(Admission {
            draft: Some(draft),
            waits: Vec::new(),
        })
    }

    fn host_state(&self) -> Result<Option<lash_vm_protocol::EncodedPayload>, String> {
        (self.host_state)()
    }
}

/// The limit a run's operations are admitted under: the tool default from
/// now on the durable store's clock.
///
/// # Errors
///
/// The store's refusal to read its clock.
pub async fn run_operation_limit(
    cx: &lash_core::ActorContext,
) -> Result<lash_sansio::ExecutionLimit, lash_core::durable_port::DurableError> {
    let budget = lash_sansio::ExecutionBudgets::default().tool_default();
    let now = cx.durable_now().await?;
    Ok(lash_sansio::ExecutionLimit::starting_at(
        u64::try_from(now.0).unwrap_or(0),
        budget,
        budget,
    ))
}
