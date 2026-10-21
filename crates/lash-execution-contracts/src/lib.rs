//! Shared execution records and persistence interfaces below the runtime.
#![deny(clippy::future_not_send)]
pub mod process;
pub use process::*;
pub mod package_gate;
pub use package_gate::{CellPackages, PackageGate, WorkflowPackages};
pub mod backend;
pub use backend::*;
pub mod module_artifacts;
pub use module_artifacts::*;
pub mod projection;
pub use projection::*;
pub mod formats;
pub mod plugin_error;
pub mod waits;
pub use plugin_error::*;
pub mod config_error;
mod controller_error;
pub mod declaration_error;
pub mod session_types;
pub use session_types::{PluginOwned, SessionCreateRequest, SessionHandle, UnstatedSessionConfig};
pub mod session_spec;
pub use session_spec::{SessionSpec, SpecResolveError};
pub mod park;
pub use park::ProcessParkReason;
mod causal;
pub use causal::process_event_invocation;
use lash_core_ids::clock::Clock;
use lash_core_store::artifact_referrer::ArtifactName;
use lash_core_store::artifact_referrer::ArtifactReferrer;
use lash_core_store::artifact_referrer::ArtifactReferrerKind;
use lash_core_store::execution_state::PluginOptions;
use lash_core_store::plugin_state::FormatRefusal;
use lash_core_store::session_identity::FrameNodeId;
use lash_core_store::tool_surface::DeferredMatch;
use lash_core_store::tool_surface::OperationLink;
use lash_protocol_types::AttachmentId;
use lash_protocol_types::CancelOrigin;
use lash_protocol_types::CancelRequest;
use lash_protocol_types::FailureCode;
use lash_protocol_types::JsonSchema;
use lash_protocol_types::ParkBound;
use lash_protocol_types::llm::types::GenerationOptions;
use lash_protocol_types::llm::types::LlmTerminalReason;
use lash_protocol_types::module_artifact_refusal::ModuleArtifactCorruption;

use lash_core_store::artifact_referrer::ReferrerClaim;
use lash_core_store::artifact_referrer::ReferrerStore;
use lash_core_store::artifact_referrer::ResolvedArtifactCleanup;
use lash_core_store::effect_identity::RuntimeInvocation;
use lash_core_store::effect_identity::RuntimeReplay;
use lash_core_store::effect_identity::RuntimeReplayAttribution;
use lash_core_store::runtime_error::RuntimeEffectReplayMismatchReport;
use lash_core_store::runtime_error::RuntimeError;
use lash_core_store::runtime_error::RuntimeErrorCode;
use lash_protocol_types::RegistrationRefused;
use lash_protocol_types::SchemaAdmissionError;

use lash_core_ids::clock::SystemClock;
use lash_core_store::process_identity::StartKeyDerivation;
use lash_core_store::tool_surface::SurfaceRefusal;
use lash_core_store::turn_input_vocabulary::TurnInput;
use lash_protocol_types::SessionAppendNode;
use lash_protocol_types::ToolCallId;
use lash_protocol_types::ToolCallOutcome;
use lash_protocol_types::ToolCallOutput;
use lash_protocol_types::ToolCancellation;
use lash_protocol_types::ToolCatalogBuildError;
use lash_protocol_types::ToolControl;
#[cfg(test)]
use lash_protocol_types::ToolDefinition;
use lash_protocol_types::ToolFailure;
use lash_protocol_types::ToolFailureClass;
use lash_protocol_types::ToolFailureSource;
use lash_protocol_types::ToolId;
use lash_protocol_types::ToolValue;
use lash_protocol_types::TurnId;
use lash_protocol_types::ValueMismatch;

use lash_core_ids::{identity_json, stable_hash, stable_identity};

use lash_core_store::compat;
#[cfg(test)]
use lash_core_store::process_id_for_test;
use lash_core_store::prompt_sections;
use lash_core_store::store;

use lash_core_llm::llm_profile::*;
use lash_core_llm::provider::*;
use lash_core_store::admitted_scope::AdmittedScope;
use lash_core_store::effect_identity::*;
use lash_core_store::effect_opener::{EffectOpener, EffectOpenerError};
use lash_core_store::runtime_error::*;
use lash_core_store::session_identity::*;
use lash_core_store::session_policy::*;

use lash_protocol_types::{ToolIntentIdentity, ToolIntentKind};

pub mod deployment;
pub use deployment::*;
pub mod deployment_decorator;
pub use deployment_decorator::*;
pub mod turn_prelude;
pub use turn_prelude::*;
pub mod tool_intent;
pub use tool_intent::*;
pub mod tool_intent_outcome;
pub use tool_intent_outcome::*;
pub mod state_commands;
use lash_core_effect::Resolution;
use lash_core_llm::session_model::ChargeSafetyPolicy;
use lash_core_store::artifact_referrer::*;
use lash_core_store::attachments::{AttachmentRootSet, AttachmentStore, AttachmentStoreError};
use lash_core_store::obligations::RegistryKind;
use lash_core_store::plugin_registrations::{PreflightIssue, ToolProviderHandle};
#[cfg(test)]
use lash_core_store::plugin_state::FormatNamespace;
use lash_core_store::plugin_state::PluginStateError;
use lash_core_store::process_identity::*;
use lash_core_store::run_spec::RunShapeRefusal;
use lash_core_store::runtime_owner::RuntimeOwner;
use lash_core_store::store::*;
use lash_core_store::turn_control_vocabulary::TurnAddress;
use lash_protocol_types::module_artifact_refusal::ModuleArtifactRefusal;
pub use state_commands::StateCommands;

pub mod module_nodes;

pub mod run;
pub mod scope;

#[cfg(any(test, feature = "testing"))]
pub mod testing;

#[cfg(test)]
use lash_core_ids::FormatVersion;

use lash_protocol_types::ProviderFailureKind;
use lash_protocol_types::{CausalRef, MaxToolCalls, NoProgressBudget, TurnBudget};

pub mod declared_start;
pub use declared_start::{DeclaredStart, DeclaredStartRefused};
