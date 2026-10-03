//! The retained tool-material store (K2/K6, FIG-4889): bundles kept alive by
//! dependency leases over the shared artifact referrer edges.
//!
//! The order the lease rules need is the order of these calls. A segment or
//! source retains a bundle, which commits its lease with the bytes, before it
//! publishes any of the bundle's references in a continuation or seal. The
//! successor or consumer acquires its own lease before it reads, and the
//! predecessor releases only once successor ownership is durable. A release
//! fences the holder, severs its leases and retires every bundle no lease
//! holds any more, in one transaction.

use crate::store::plugin_writers::PluginRevision;
use crate::tool_run::{
    MaterialBundle, MaterialHolder, MaterialOwner, MaterialPayload, MaterialRef,
    MaterialRetentionError, RetainedBundle,
};

/// Retained tool-material bundles of one durable core.
#[async_trait::async_trait]
pub trait ToolMaterialStore: Send + Sync {
    /// Store `bundle` if absent and add `holder`'s lease, in one transaction.
    /// Retaining a stored bundle again is the same fact.
    ///
    /// # Errors
    ///
    /// [`MaterialRetentionError::HolderEnded`] once `holder` is fenced, and
    /// store failures.
    async fn retain_material(
        &self,
        holder: &MaterialHolder,
        bundle: &MaterialBundle,
    ) -> Result<RetainedBundle, MaterialRetentionError>;

    /// Add `holder`'s lease to a bundle another holder retained: a
    /// successor's or consumer's edge acquire, before any read. Answers the
    /// bundle as `holder` now holds it.
    ///
    /// # Errors
    ///
    /// [`MaterialRetentionError::HolderEnded`], a typed `Missing` refusal
    /// once the bundle has retired, and store failures.
    async fn acquire_material(
        &self,
        holder: &MaterialHolder,
        bundle: &RetainedBundle,
    ) -> Result<RetainedBundle, MaterialRetentionError>;

    /// End `holder` for good: fence it, sever every lease it holds and
    /// retire each bundle no lease holds any more, in one transaction.
    /// Releasing an ended holder is a no-op.
    ///
    /// # Errors
    ///
    /// Store failures; nothing is applied.
    async fn release_material(&self, holder: &MaterialHolder)
    -> Result<(), MaterialRetentionError>;

    /// Read the payload `reference` names under `holder`'s lease, verified
    /// against the reader's `owner`, its integrity, its format and the
    /// `available` codec revisions.
    ///
    /// # Errors
    ///
    /// A typed refusal: `Retired` once `holder` has ended, `Missing` when it
    /// holds no lease on the reference's bundle or the bundle has no such
    /// payload, otherwise as [`MaterialPayload::verify`]. Store failures.
    async fn read_material(
        &self,
        holder: &MaterialHolder,
        reference: &MaterialRef,
        owner: &MaterialOwner,
        available: &[PluginRevision],
    ) -> Result<MaterialPayload, MaterialRetentionError>;
}
