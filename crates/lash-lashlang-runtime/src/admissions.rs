//! What admitting a run's operations records (ADR 0132 §5, §8).

use lash_vm_broker::{Admission, MemberDraft, OperationRequest};

use crate::OperationAdmissions;

/// The host that knows what an operation's tool calls are: their tools,
/// requests and declarations.
#[async_trait::async_trait]
pub trait MemberAdmissions: Send + Sync {
    /// The member executions of operation `ordinal`, which `request` asks
    /// for: every tool call it makes, under the policy, limit and
    /// completion wait its tool declares, with the request its body is
    /// built from again. None for an operation that makes no tool call, or
    /// whose calls the host refuses before any is admitted.
    ///
    /// # Errors
    ///
    /// Why the operation cannot be admitted; the run stops.
    async fn members(
        &self,
        ordinal: u64,
        request: &OperationRequest,
    ) -> Result<Vec<MemberDraft>, String>;
}

/// The admissions of one run's operations. Every tool call an operation
/// makes, alone or in an aggregate, is its own admitted execution, as its
/// host declares it (ADR 0132 §5). A wait (an await, a sleep)
/// is admitted as no execution: a restore performs it again. A sleep pins
/// its timer with its admission, due at the absolute deadline it was
/// admitted with, and so does each timer leaf of an aggregate, in leaf
/// order, so a restore races those timers and never waits their whole
/// duration again (ADR 0132 §6).
pub struct RunAdmissions<'a> {
    /// The actor whose store clock dates a timer's deadline, and whose
    /// execution scope revokes it.
    pub cx: &'a lash_core::ActorContext,
    /// The host that admits an operation's tool calls.
    pub members: &'a dyn MemberAdmissions,
    /// The host's own state at a quiet point (a cell's envelope), committed
    /// with the VM's snapshot.
    pub host_state:
        &'a (dyn Fn() -> Result<Option<lash_vm_protocol::EncodedPayload>, String> + Send + Sync),
}

impl RunAdmissions<'_> {
    async fn timer(&self, spec: lash_core::SleepSpec) -> Result<lash_vm_broker::WaitSpec, String> {
        lash_core::waits::timer(self.cx, spec)
            .await
            .map_err(|error| error.to_string())
    }
}

#[async_trait::async_trait]
impl OperationAdmissions for RunAdmissions<'_> {
    async fn admission(
        &self,
        ordinal: u64,
        request: &OperationRequest,
    ) -> Result<Admission, String> {
        if let OperationRequest::Sleep(sleep) = request {
            // A sleep its host refuses (its value is no duration or
            // deadline) pins nothing: the host answers the guest the
            // refusal.
            let Ok(spec) = crate::bridge::process_sleep(sleep.kind, &sleep.value) else {
                return Ok(Admission::default());
            };
            return Ok(Admission {
                members: Vec::new(),
                waits: vec![self.timer(spec).await?],
            });
        }
        if lash_vm_broker::waits_only(request) {
            return Ok(Admission::default());
        }
        let mut waits = Vec::new();
        if let OperationRequest::ResourceOperationBatch(batch) = request {
            for leaf in &batch.leaves {
                // A timer leaf its host refuses pins nothing; the host
                // settles it with the refusal.
                if let lashlang::ResourceOperationBatchLeaf::Timer(sleep) = leaf
                    && let Ok(duration_ms) = crate::timer_duration_ms(sleep)
                {
                    waits.push(
                        self.timer(lash_core::SleepSpec::For { duration_ms })
                            .await?,
                    );
                }
            }
        }
        Ok(Admission {
            members: self.members.members(ordinal, request).await?,
            waits,
        })
    }

    fn host_state(&self) -> Result<Option<lash_vm_protocol::EncodedPayload>, String> {
        (self.host_state)()
    }
}
