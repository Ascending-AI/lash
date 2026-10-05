//! ADR 0130: a committed final's intent realization runs in its own durable
//! invocation, with its own journal. The Run's journal records only the
//! realization's admission and, later, the receipt the realization answers.
//!
//! The Run sends one [`RealizationRequest`] under its [`RealizationKey`]'s
//! idempotency and attaches to the invocation the send created; both commands
//! sit at deterministic positions of the Run's program. A [`ToolRealizer`]
//! performs the intents inside that invocation, journaling through the scope
//! the request carries, and answers a [`RealizationReceipt`].
//!
//! [`RealizationKey`]: crate::tool_run::RealizationKey

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

/// The process-side facts the realization invocation needs to run a final's
/// intents exactly as the Run's own dispatch would have: the owner they are
/// charged to, the invocation they are attributed under, the lineage and
/// originator their starts inherit, and the environment and plugin admission
/// their work resolves against.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealizationDispatch {
    pub owner: crate::ExecutionOwner,
    pub parent_invocation: Option<crate::RuntimeInvocation>,
    pub process_lineage: Option<crate::ProcessLineage>,
    pub process_originator: Option<crate::ProcessOriginator>,
    pub environment: crate::ProcessExecutionEnvSpec,
    pub plugin_admission: Option<crate::store::plugin_writers::PluginAdmission>,
}

/// What a final's declarations send to their realization invocation. The
/// payload is deterministic from the call's admitted facts; the send itself
/// is journaled, so a replay serves it rather than resending.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealizationPayload {
    pub intents: crate::ToolIntents,
    /// The dispatch the intents realize under. `None` carries nothing a
    /// process-scoped intent could use: only a realizer serving intents that
    /// need no dispatch facts may answer it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatch: Option<RealizationDispatch>,
}

/// The Run's request to the realization service. `key` is the send's
/// idempotency key and the realized invocation's identity;
/// `scope` is the Run's admitted execution scope, so the invocation's
/// controller mints the intent identities, effect addresses and referrer
/// claims the Run's own would have been.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealizationRequest {
    pub key: crate::tool_run::RealizationKey,
    #[serde(with = "crate::admitted_scope_wire")]
    pub scope: crate::AdmittedScope,
    pub call_id: ToolCallId,
    pub payload: RealizationPayload,
}

/// The realization's answer, selected by the Run's schedule and recorded as
/// `Realized`'s receipt material: the ordered outcome of every intent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RealizationReceipt {
    pub outcomes: Vec<crate::ToolIntentExecutionOutcome>,
}

/// The realizer a realization invocation drives. The production one is the
/// deployment's process worker; tests install their own. `realize` runs
/// again on every replay of the realization invocation — its own journal
/// serves its recorded commands — and a duplicate send under the key
/// reaches the first invocation instead of starting another.
#[async_trait::async_trait]
pub trait ToolRealizer: Send + Sync {
    /// Realize one admitted final's intents inside the realization
    /// invocation, journaling every durable command it issues through
    /// `scoped`, the invocation's own journal.
    async fn realize(
        &self,
        request: RealizationRequest,
        scoped: crate::ScopedEffectController<'_>,
    ) -> Result<RealizationReceipt, crate::RuntimeEffectControllerError>;
}

/// A realization already admitted to the engine. Only its durable identity
/// crosses a physical cut; its selectable belongs to this invocation.
pub struct IssuedRealization<'run> {
    pub invocation_id: String,
    pub receipt: super::RunSelectable<'run, RealizationReceipt>,
}
