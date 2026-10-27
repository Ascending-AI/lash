//! Diagnostic projections only: errors retain their typed cause and authority.
use crate::EmbedError;
use lash_execution_contracts::PluginError;
use lash_vm_client::service::SourceServiceError;
use tracing::Instrument as _;

pub(super) async fn phase<T>(
    phase: &'static str,
    span: tracing::Span,
    operation: impl std::future::Future<Output = crate::Result<T>>,
) -> crate::Result<T> {
    async { operation.await.inspect_err(|error| report(phase, error)) }
        .instrument(span)
        .await
}

pub(crate) fn refused(phase: &str, refusal_kind: &str) {
    tracing::debug!(event = "artifact_refused", phase, refusal_kind);
}

pub(crate) fn source_refused(phase: &str, error: &SourceServiceError) {
    match error {
        SourceServiceError::Dialect(refusal) => refused(phase, refusal.code()),
        SourceServiceError::UnknownDialect { .. } => refused(phase, "dialect_unavailable"),
        SourceServiceError::UndeclaredMembers { .. } => refused(phase, "undeclared_members"),
        SourceServiceError::Print(error) => {
            use crate::dialect::PrintError;
            refused(
                phase,
                match error {
                    PrintError::Namespace(_) => "generated_source_namespace",
                    PrintError::Bytes { .. } => "generated_source_bytes",
                    PrintError::Depth { .. } => "generated_source_depth",
                    PrintError::Encode(_) => "generated_source_encode",
                    PrintError::Unprintable { .. } => "generated_source_unprintable",
                    PrintError::Invalid(_) => "generated_source_invalid",
                },
            );
        }
        SourceServiceError::Worker(_) => {}
    }
}

fn report(phase: &str, error: &EmbedError) {
    if !tracing::enabled!(tracing::Level::DEBUG) {
        return;
    }
    if let Some(refusal) = error.artifact_refusal() {
        refused(phase, refusal.code());
        return;
    }
    match error {
        EmbedError::WorkflowSource(error) => source_refused(phase, error),
        EmbedError::WorkflowPublication { source, .. } => engine_refused(phase, source),
        EmbedError::WorkflowBytes(_) => refused(phase, "document_bytes"),
        EmbedError::WorkflowSourceMap(_) => refused(phase, "source_map"),
        EmbedError::ConflictingOperationRoutes { .. } => {
            refused(phase, "conflicting_operation_routes")
        }
        EmbedError::Plugin(PluginError::Package(error)) => engine_refused(phase, error),
        EmbedError::Plugin(PluginError::ProcessEnvRefused { .. }) => {
            refused(phase, "process_env_refused")
        }
        EmbedError::Plugin(PluginError::PackageStoreRefused(error)) => {
            package_refused(phase, error)
        }
        _ => {}
    }
}

fn package_refused(phase: &str, error: &lash_core_store::packages::PackageRefusal) {
    use lash_core_store::packages::PackageRefusal;
    refused(
        phase,
        match error {
            PackageRefusal::SessionNotLive(_) => "session_not_live",
            PackageRefusal::Identity(_) => "package_identity",
            PackageRefusal::Immutable(_) => "package_immutable",
            PackageRefusal::Missing(_) => "package_missing",
            PackageRefusal::CarryMissing { .. } => "package_carry_missing",
            PackageRefusal::Publication => "package_publication",
            PackageRefusal::Cycle => "package_cycle",
            PackageRefusal::ExecutionUse => "package_execution_use",
            PackageRefusal::ResolutionTarget => "package_resolution_target",
            PackageRefusal::ReferrerEnded(_) => "pin_released",
        },
    );
}

fn engine_refused(
    phase: &str,
    error: &lash_execution_contracts::process::linking::EnginePortError,
) {
    use lash_execution_contracts::process::linking::{AdmissionRefusal, EnginePortError};
    match error {
        EnginePortError::Refused(error) => refused(
            phase,
            match error {
                AdmissionRefusal::CallableIdentity { .. } => "callable_identity",
                AdmissionRefusal::EntryAnnotation { .. } => "entry_annotation",
                AdmissionRefusal::SignatureRequired { .. } => "signature_required",
                AdmissionRefusal::CallableTooLarge { .. } => "callable_too_large",
                AdmissionRefusal::UnpublishedOrigin { .. } => "unpublished_origin",
                AdmissionRefusal::MissingImport { .. } => "missing_import",
                AdmissionRefusal::KindMismatch { .. } => "kind_mismatch",
                AdmissionRefusal::SignatureMismatch { .. } => "signature_mismatch",
                AdmissionRefusal::ConventionMismatch { .. } => "convention_mismatch",
                AdmissionRefusal::NotVisible { .. } => "not_visible",
                AdmissionRefusal::PathConflict { .. } => "path_conflict",
                AdmissionRefusal::NotOwner { .. } => "not_owner",
                AdmissionRefusal::UndeclaredDeferred { .. } => "undeclared_deferred",
                AdmissionRefusal::ProviderNotHostable { .. } => "provider_not_hostable",
                AdmissionRefusal::MissingRoute { .. } => "missing_route",
                AdmissionRefusal::ContractMismatch { .. } => "contract_mismatch",
                AdmissionRefusal::MissingMain => "missing_main",
                AdmissionRefusal::NotFunction { .. } => "not_function",
                AdmissionRefusal::NotDataOnly { .. } => "not_data_only",
                AdmissionRefusal::InvalidDocument { .. } => "invalid_document",
                AdmissionRefusal::IncompatibleSuccession { .. } => "incompatible_succession",
            },
        ),
        EnginePortError::PackageRefused(error) => package_refused(phase, error),
        EnginePortError::Missing { .. } => refused(phase, "artifact_missing"),
        EnginePortError::Corrupt { .. } => refused(phase, "artifact_corrupt"),
        EnginePortError::Fenced => refused(phase, "pin_released"),
        EnginePortError::Unsupported { .. } => refused(phase, "unsupported"),
        EnginePortError::StoreRefused(_) => refused(phase, "store_refused"),
        EnginePortError::StoreUnavailable { .. } | EnginePortError::Unavailable { .. } => {}
    }
}
