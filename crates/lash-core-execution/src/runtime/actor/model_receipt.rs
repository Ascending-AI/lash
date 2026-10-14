//! A model call's receipt as its root stores it (FIG-6253): one placed
//! text, named by `turn_phases.answer_ref` beside the model phase that pins
//! the call, from its `model.answer` commit until the turn's next phase
//! consumes it.
//!
//! The receipt is immutable and stands on its own: recording it leaves the
//! phase and its PhaseRoot as they are, so it costs its own nodes (one
//! TextLeaf, until it outgrows a leaf) and the row's reference, never a new
//! phase root. Its JSON text states the callbacks its derivation owes (S7)
//! beside the outcome, so a claim reads its debts without the integrator's
//! types.

use lash_core_store::checkpoint_graph::{
    DecodeLimits, GraphDecoder, GraphStaging, NodeId, NodeSource,
};

use crate::plugin::PluginCallbackIdentity;

/// A model call's receipt: the callbacks its derivation owes, in their
/// recorded order, and the outcome `A` the integrator records.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelReceipt<A> {
    /// The callbacks a node must resolve to derive the receipt (S7): what
    /// the turn's phase owes while the receipt stands.
    pub owed: Vec<PluginCallbackIdentity>,
    /// The provider's outcome as the integrator records it.
    pub answer: A,
}

impl<A: serde::Serialize> ModelReceipt<A> {
    /// Stages the receipt as its placed text and returns its root.
    ///
    /// # Errors
    ///
    /// The account of a receipt that does not encode or stage.
    pub fn stage(&self, staging: &mut GraphStaging) -> Result<NodeId, String> {
        let text = serde_json::to_string(self)
            .map_err(|error| format!("the model call's receipt does not encode: {error}"))?;
        staging
            .place_text(&text)
            .map_err(|error| format!("the model call's receipt does not stage: {error}"))
    }
}

impl<A: serde::de::DeserializeOwned> ModelReceipt<A> {
    /// Reads receipt `root` from `source`.
    ///
    /// # Errors
    ///
    /// The account of a root that does not read or a text that does not
    /// decode.
    pub fn load(
        source: &dyn NodeSource,
        root: NodeId,
        limits: DecodeLimits,
    ) -> Result<Self, String> {
        let text = GraphDecoder::new(source, limits)
            .text(root)
            .map_err(|error| error.to_string())?;
        serde_json::from_str(&text)
            .map_err(|error| format!("the model call's receipt does not decode: {error}"))
    }
}
