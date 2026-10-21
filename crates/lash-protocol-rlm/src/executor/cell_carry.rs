//! Linked carry requires a complete runtime correspondence before any write.
use super::envelope::CellEnvelope;
use lash_core_store::checkpoint_graph::{GraphDecoder, NodeKind};

pub(super) async fn carry(
    snapshots: &lash_vm_broker::DurableSnapshotStore,
    kernel: &super::KernelCarry,
    _bounds: lash_kernel_vm::Bounds,
) -> Result<(), Error> {
    let Some(read) = snapshots
        .latest_root()
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    if read.body.status != lash_core_store::checkpoint_graph::bodies::ExecStatus::Parked {
        return Ok(());
    }
    let mut decoder = GraphDecoder::new(&read.graph, read.read_limits);
    let envelope = CellEnvelope::load(
        &mut decoder,
        read.body.envelope.ok_or("a parked cell has no envelope")?,
    )
    .map_err(|error| error.to_string())?;
    if envelope.document.kernel() == Some(kernel.writes().number()) {
        return Ok(());
    }
    Err(Error::Migration(lash_vm_runtime::KernelMigrationRefusal::NotAdmitted {
        document: envelope.document.id(),
        reason: "linked carry requires a complete retained Link DAG correspondence; no linked migration is installed".into(),
    }))
}

/// Refusals keep their serialized cause across the generic plugin boundary;
/// infrastructure aborts the attempt without recording a cell outcome.
#[derive(Debug, thiserror::Error)]
pub(super) enum Error {
    #[error(transparent)]
    Carry(lash_kernel_migrate::CarryFault),
    #[error(transparent)]
    Migration(lash_vm_runtime::KernelMigrationRefusal),
    #[error("{0}")]
    Host(String),
}
impl From<String> for Error {
    fn from(message: String) -> Self {
        Self::Host(message)
    }
}
impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Self::Host(message.into())
    }
}
impl Error {
    pub(super) fn into_runtime(self) -> lash_core::RuntimeEffectControllerError {
        use lash_protocol_types::{PluginFailureClass, PluginOperationFailure};
        let code = lash_core::RuntimeErrorCode::ExecutionStateCaptureFailed;
        let message = self.to_string();
        let typed = match &self {
            Self::Carry(lash_kernel_migrate::CarryFault::Refused(refusal)) => Some((
                "lash.kernel.parked_carry",
                serde_json::to_value(refusal),
                PluginFailureClass::Parked,
            )),
            Self::Migration(refusal) => Some((
                "lash.kernel.migration",
                serde_json::to_value(refusal),
                PluginFailureClass::Parked,
            )),
            Self::Carry(lash_kernel_migrate::CarryFault::Heap(fault)) => {
                use lash_kernel_doc::heap::HeapFault;
                let reason = match fault {
                    HeapFault::Missing { .. } => "missing",
                    HeapFault::Corrupt { .. } => "corrupt",
                    HeapFault::Unauthorized { .. } => "unauthorized",
                    HeapFault::LeaseLost => "lease_lost",
                    HeapFault::Exhausted { .. } => "exhausted",
                    HeapFault::Transport(_) => "transport",
                    HeapFault::Malformed(_) => "malformed",
                    _ => "unknown",
                };
                Some((
                    "lash.kernel.heap",
                    Ok(serde_json::json!({"reason": reason, "detail": fault.to_string()})),
                    PluginFailureClass::Redrivable,
                ))
            }
            _ => None,
        };
        let mut error = lash_core::RuntimeError::new(code.clone(), message.clone());
        if let Some((error_type, payload, class)) = typed {
            match payload {
                Ok(payload) => {
                    error.cause = Some(lash_core::RuntimeErrorCause::PluginOperation {
                        failure: Box::new(PluginOperationFailure {
                            error_type: error_type.into(),
                            error_version: std::num::NonZeroU32::MIN,
                            payload,
                            class,
                            code: (&code).into(),
                            message,
                            origin: None,
                        }),
                    })
                }
                Err(fault) => {
                    return lash_core::RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::RecordEncodingFailed,
                        fault.to_string(),
                    );
                }
            }
        }
        lash_core::RuntimeEffectControllerError::from(error).retryable_uncommitted_derivation()
    }
}

#[cfg(test)]
mod tests {
    /// A semantic refusal keeps its discriminant and detail through the
    /// plugin payload; it parks the turn instead of becoming a cell result.
    #[test]
    fn a_cell_carry_refusal_keeps_its_typed_cause_at_the_plugin_boundary() {
        let refusal = lash_kernel_migrate::ParkedRefusal::StateNotCarried {
            detail: "pinned snapshot R1 lacks function f".into(),
        };
        let error = super::Error::Carry(refusal.clone().into()).into_runtime();
        let Some(lash_core::RuntimeErrorCause::PluginOperation { failure }) = error.cause else {
            panic!("{error:?}");
        };
        assert_eq!(
            failure.class,
            lash_protocol_types::PluginFailureClass::Parked
        );
        assert_eq!(
            serde_json::from_value::<lash_kernel_migrate::ParkedRefusal>(failure.payload).unwrap(),
            refusal
        );
    }
}
