//! Compile witness for the ADR 0079 facade: every path the host (figments)
//! imports from a `lash-internal-*` crate, reached through `lash::` alone.
//!
//! The inventory is the set of distinct paths recorded in the FIG-3189
//! evidence sweep of the host tree. A path that names an inherent associated
//! item (`Type::new`, `Type::default`) or an enum variant
//! (`Enum::Variant`) is witnessed by its owning type: the facade decides where
//! the type lives, not what hangs off it. Paths that no longer exist anywhere
//! in this workspace (the host pins an older revision) are listed in the pull
//! request rather than witnessed here.
//!
//! Imports bind to `_` on purpose: this file proves that the paths resolve,
//! and binding no names keeps it free of ordering and shadowing accidents.
//!
//! `as _` silences `unused_imports` only for a trait, whose methods enter
//! scope through the anonymous binding. Most rows here name a struct, an
//! enum or a function, so the binding is genuinely unused and the lint
//! fires: without the crate-level allow below, `//:workspace_clippy`
//! (`-Dwarnings`) reports 152 errors. The allow covers `unused_imports`
//! alone; a path that stops resolving is still E0432, a hard error.

#![allow(unused_imports)]

// --- Ungated facade: lash-internal-core, -sansio, -trace, -remote-protocol ---

use lash::ModelSpec as _;
use lash::PendingTurnInput as _;
use lash::PendingTurnInputCancelOutcome as _;
use lash::PendingTurnInputSuffixCancelOutcome as _;
use lash::TurnActivity as _;
use lash::TurnCancelRequest as _;
use lash::TurnEvent as _;
use lash::TurnOutcome as _;
use lash::TurnStop as _;
use lash::TurnWorkDriver as _;
use lash::attachments::AttachmentCreateMeta as _;
use lash::attachments::AttachmentId as _;
use lash::attachments::AttachmentRef as _;
use lash::attachments::MediaType as _;
use lash::direct::GenerationOptions as _;
use lash::direct::GenerationOptions as _;
use lash::direct::LlmOutputPart as _;
use lash::direct::LlmTerminalReason as _;
use lash::direct::LlmUsage as _;
use lash::durability::BoundaryReason as _;
use lash::durability::ensure_durable_effect_input as _;
use lash::observe::InMemoryLiveReplayStore as _;
use lash::persistence::CheckpointKind as _;
use lash::persistence::InMemorySessionStoreFactory as _;
use lash::persistence::PendingTurnInputDraft as _;
use lash::persistence::SessionAttachmentStore as _;
use lash::persistence::SessionRelation as _;
use lash::persistence::SessionStoreCreateRequest as _;
use lash::persistence::TurnInputCheckpointBoundary as _;
use lash::persistence::TurnInputIngress as _;
use lash::persistence::TurnInputState as _;
use lash::persistence::reclaim_unreferenced_attachments as _;
use lash::plugins::ContextError as _;
use lash::plugins::PluginError as _;
use lash::plugins::PluginOptions as _;
use lash::plugins::PreparedContext as _;
use lash::plugins::SegmentHandover as _;
use lash::plugins::ToolCatalog as _;
use lash::plugins::TurnContextTransform as _;
use lash::plugins::TurnTransformContext as _;
use lash::process::CausalRef as _;
use lash::process::ProcessAwaitOutput as _;
use lash::process::ProcessCompletionAuthority as _;
use lash::process::ProcessEventAppendRequest as _;
use lash::process::ProcessExecutionEnvRef as _;
use lash::process::ProcessExecutionEnvSpec as _;
use lash::process::ProcessIdentity as _;
use lash::process::ProcessInput as _;
use lash::process::ProcessListFilter as _;
use lash::process::ProcessOriginator as _;
use lash::process::ProcessProvenance as _;
use lash::process::ProcessPruneReport as _;
use lash::process::ProcessRecord as _;
use lash::process::ProcessRef as _;
use lash::process::ProcessRegistration as _;
use lash::process::ProcessStartRequest as _;
use lash::process::ProcessStatusFilter as _;
use lash::process::SessionScope as _;
use lash::provider::CacheControlDialect as _;
use lash::provider::LlmContentBlock as _;
use lash::provider::LlmRequest as _;
use lash::provider::LlmRequest as _;
use lash::provider::LlmRequestScope as _;
use lash::provider::LlmResponse as _;
use lash::provider::ModelCapability as _;
use lash::provider::ModelCapability as _;
use lash::provider::ModelEffortValidationCategory as _;
use lash::provider::ProviderCompletion as _;
use lash::provider::ProviderComponents as _;
use lash::provider::ProviderFailureKind as _;
use lash::provider::ProviderReliability as _;
use lash::provider::ReasoningCapability as _;
use lash::provider::ReasoningDisableEncoding as _;
use lash::provider::ReasoningSelection as _;
use lash::provider::ReasoningSelection as _;
use lash::provider::StreamTermination as _;
use lash::remote::llm::RemoteSchemaContract as _;
use lash::remote::llm::RemoteSchemaProjectionPolicy as _;
use lash::runtime::Clock as _;
use lash::runtime::RuntimeEffectController as _;
use lash::runtime::RuntimeError as _;
use lash::runtime::RuntimeErrorCode as _;
use lash::runtime::ScopedEffectController as _;
use lash::runtime::SessionPolicy as _;
use lash::runtime::SystemClock as _;
use lash::runtime::current_epoch_ms as _;
use lash::tools::ToolId as _;
use lash::tools::ToolProvider as _;
use lash::tools::ToolRegistry as _;
use lash::tools::ToolRetryPolicy as _;
use lash::tools::ToolSourceHandle as _;
use lash::tracing::TraceContext as _;
use lash::tracing::TracePromptComponent as _;
use lash::tracing::TraceTokenUsage as _;
use lash::tracing::TraceToolCallOutcome as _;
use lash::tracing::TraceToolCallOutput as _;
use lash::triggers::InMemoryTriggerStore as _;
use lash::triggers::LashSchema as _;
use lash::triggers::TriggerDeliveryReservation as _;
use lash::triggers::TriggerInputBinding as _;
use lash::triggers::TriggerOccurrenceFilter as _;
use lash::triggers::TriggerOccurrenceRecord as _;
use lash::triggers::TriggerOccurrenceRequest as _;
use lash::triggers::TriggerRegistration as _;
use lash::triggers::TriggerSubscriptionDraft as _;
use lash::triggers::TriggerSubscriptionFilter as _;
use lash::triggers::TriggerSubscriptionRecord as _;

/// `lash_sansio::schema_contract`, the one sans-io module the host names.
use lash::schema::{SchemaContract as _, SchemaProjectionPolicy as _};

// --- `rlm`: the Lashlang protocol, runtime and language surface ---

#[cfg(feature = "rlm")]
mod rlm_inventory {
    use lash::rlm::LashlangAbilities as _;
    use lash::rlm::LashlangHostEnvironment as _;
    use lash::rlm::LinkedModule as _;
    use lash::rlm::ModuleCompileOutput as _;
    use lash::rlm::NamedDataType as _;
    use lash::rlm::TypeExpr as _;
    use lash::rlm::TypeField as _;
    use lash::tools::link_with_deferred_resolution as _;

    // The Lashlang language vocabulary, re-exported whole as `lash::rlm::lang`.
    use lash::rlm::lang::AbilityOp as _;
    use lash::rlm::lang::AbilityResult as _;
    use lash::rlm::lang::ContentHash as _;
    use lash::rlm::lang::DurabilityTier as _;
    use lash::rlm::lang::ExecutionEnvironment as _;
    use lash::rlm::lang::ExecutionHost as _;
    use lash::rlm::lang::ExecutionHostError as _;
    use lash::rlm::lang::Expr as _;
    use lash::rlm::lang::HostDescriptor as _;
    use lash::rlm::lang::HostRequirementsRef as _;
    use lash::rlm::lang::ImageValue as _;
    use lash::rlm::lang::ListComprehensionClause as _;
    use lash::rlm::lang::ModuleCompileRequest as _;
    use lash::rlm::lang::ModuleIntrospection as _;
    use lash::rlm::lang::ModuleRef as _;
    use lash::rlm::lang::NamedDataTypeIntrospection as _;
    use lash::rlm::lang::ProcessIntrospection as _;
    use lash::rlm::lang::ResourceOperation as _;
    use lash::rlm::lang::ResourceOperationBatchResult as _;
    use lash::rlm::lang::ResourceOperationResult as _;
    use lash::rlm::lang::State as _;
    use lash::rlm::lang::TriggerInputTemplate as _;
    use lash::rlm::lang::TriggerListRequest as _;
    use lash::rlm::lang::TriggerRegistrationRequest as _;
    use lash::rlm::lang::Value as _;
    use lash::rlm::lang::add_trigger_resource_operations as _;
    use lash::rlm::lang::compile_linked as _;
    use lash::rlm::lang::compile_module as _;
    use lash::rlm::lang::execute as _;
    use lash::rlm::lang::from_json as _;
}

// --- `testing`: embedder test helpers ---

#[cfg(feature = "testing")]
mod testing_inventory {
    use lash::testing::TestLocalProcessRegistry as _;
    use lash::testing::TestProvider as _;
    use lash::testing::mock_tool_context_with_execution_binding as _;
}

#[cfg(all(feature = "rlm", feature = "testing"))]
mod rlm_testing_inventory {
    // `lash_core::testing::conformance`: the runtime rebuild certification the
    // host runs, reached through the facade's own `testing` module.
    use lash::testing::conformance::{
        RuntimeRebuildBackend as _, runtime_rebuild_and_worker_recovery as _,
    };

    use lash::rlm::lang::testing::conformance::ReopenableLashlangArtifactStore as _;
    // The host names `lashlang_artifact_store_reopenable`; the live name of the
    // same conformance entry point is `survives_reopen`.
    use lash::rlm::lang::testing::conformance::survives_reopen as _;
}

// --- Host-wired extension features: one module per feature ---

#[cfg(feature = "sqlite")]
mod sqlite_inventory {
    use lash::sqlite::SqliteDatabase as _;
    use lash::sqlite::SqliteSessionStoreFactory as _;
}

#[cfg(feature = "postgres")]
mod postgres_inventory {
    use lash::postgres::PostgresSessionStoreFactory as _;
    use lash::postgres::PostgresStorage as _;
}

#[cfg(feature = "s3")]
mod s3_inventory {
    use lash::s3::S3AttachmentStore as _;
    use lash::s3::S3AttachmentStoreConfig as _;
}

#[cfg(feature = "restate")]
mod restate_inventory {
    use lash::restate::RestateEffectHost as _;
}

#[cfg(feature = "openai")]
mod openai_inventory {
    use lash::openai::OpenAiCompatibleProvider as _;
    use lash::openai::OpenAiProvider as _;
}

#[cfg(feature = "anthropic")]
mod anthropic_inventory {
    use lash::anthropic::AnthropicProvider as _;
}

#[cfg(feature = "google")]
mod google_inventory {
    use lash::google::GoogleOAuthProvider as _;
}

#[cfg(feature = "mcp")]
mod mcp_inventory {
    use lash::mcp::McpError as _;
    use lash::mcp::McpServerConfig as _;
}

#[cfg(feature = "subagents")]
mod subagents_inventory {
    use lash::subagents::SubagentsPluginFactory as _;
}

#[cfg(feature = "typescript")]
mod typescript_inventory {
    use lash::typescript::{compile as _, parse as _};
}

#[cfg(feature = "http-transport")]
mod http_transport_inventory {
    use lash::http_transport::ReqwestClient as _;
}
