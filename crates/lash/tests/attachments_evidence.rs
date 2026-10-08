//! Compile-time witnesses for attachment-area facade and integrator contracts.
//!
//! Hosts name durable refs and transient delivery contracts through `lash::`.
//! The witnesses retain the storage and referrer seams used by integrators.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T: ?Sized>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn drain_area_witnesses() {
    // W0001: lash::InputItem::Attachment [variant]
    variant_witness(|value: &lash::InputItem| matches!(value, lash::InputItem::Attachment { .. }));
    // W0002: lash::InputItem::Attachment::reference [field]
    field_witness(|value: &lash::InputItem| {
        if let lash::InputItem::Attachment { reference, .. } = value {
            let _: &lash::attachments::AttachmentRef = reference;
        }
    });
    // W0003: lash::InputItem::attachment [function]
    let _ = lash::InputItem::attachment;
    // W0004: lash::TurnInput::with_attachment [function]
    let _ = lash::TurnInput::with_attachment;
    // W0005: lash::attachments::AttachmentCreateMeta::label [field]
    field_witness(|value: &lash::attachments::AttachmentCreateMeta| {
        let _ = &value.label;
    });
    // W0020: lash::direct::DirectPart::Attachment [variant]
    variant_witness(|value: &lash::direct::DirectPart| {
        matches!(value, lash::direct::DirectPart::Attachment(..))
    });
    // W0021: lash::direct::DirectPart::Attachment::0 [field]
    field_witness(|value: &lash::direct::DirectPart| {
        if let lash::direct::DirectPart::Attachment(f0) = value {
            let _: &Box<lash::attachments::AttachmentRef> = f0;
        }
    });
    // W0025: lash::persistence::AttachmentRootSet::has_live_attachment_ref [function]
    fn meth_0025<T: lash::persistence::AttachmentRootSet>(_: &T) {
        let _ = T::has_live_attachment_ref;
    }
    // W0026: lash::persistence::AttachmentRootSet::live_attachment_refs [function]
    fn meth_0026<T: lash::persistence::AttachmentRootSet>(_: &T) {
        let _ = T::live_attachment_refs;
    }
    // W0027: lash::persistence::AttachmentStore::delete [function]
    fn meth_0027<T: lash::persistence::AttachmentStore>(_: &T) {
        let _ = T::delete;
    }
    // W0028: lash::persistence::AttachmentStore::get [function]
    fn meth_0028<T: lash::persistence::AttachmentStore>(_: &T) {
        let _ = T::get;
    }
    // W0029: lash::persistence::AttachmentStore::head [function]
    fn meth_0029<T: lash::persistence::AttachmentStore>(_: &T) {
        let _ = T::head;
    }
    // W0030: lash::persistence::AttachmentStore::list [function]
    fn meth_0030<T: lash::persistence::AttachmentStore>(_: &T) {
        let _ = T::list;
    }
    // W0031: lash::persistence::AttachmentStore::persistence [function]
    fn meth_0031<T: lash::persistence::AttachmentStore>(_: &T) {
        let _ = T::persistence;
    }
    // W0032: lash::persistence::AttachmentStore::put [function]
    fn meth_0032<T: lash::persistence::AttachmentStore>(_: &T) {
        let _ = T::put;
    }
    // W0033: lash::persistence::AttachmentStoreError::Backend [variant]
    variant_witness(|value: &lash::persistence::AttachmentStoreError| {
        matches!(
            value,
            lash::persistence::AttachmentStoreError::Backend { .. }
        )
    });
    // W0035: lash::persistence::AttachmentStoreError::Io [variant]
    variant_witness(|value: &lash::persistence::AttachmentStoreError| {
        matches!(value, lash::persistence::AttachmentStoreError::Io { .. })
    });
    // W0036: lash::persistence::AttachmentStoreError::Io::path [field]
    field_witness(|value: &lash::persistence::AttachmentStoreError| {
        if let lash::persistence::AttachmentStoreError::Io { path, .. } = value {
            let _ = path;
        }
    });
    // W0037: lash::persistence::AttachmentStoreError::Io::source [field]
    field_witness(|value: &lash::persistence::AttachmentStoreError| {
        if let lash::persistence::AttachmentStoreError::Io { source, .. } = value {
            let _ = source;
        }
    });
    // W0038: lash::persistence::AttachmentStoreError::ReferrersOperationFailed [variant]
    variant_witness(|value: &lash::persistence::AttachmentStoreError| {
        matches!(
            value,
            lash::persistence::AttachmentStoreError::ReferrersOperationFailed { .. }
        )
    });
    // W0039: the manifest failure retains its operation, attachment id and typed cause.
    field_witness(|value: &lash::persistence::AttachmentStoreError| {
        if let lash::persistence::AttachmentStoreError::ReferrersOperationFailed {
            operation,
            attachment_id,
            source,
        } = value
        {
            let _: &&'static str = operation;
            let _: &lash::attachments::AttachmentId = attachment_id;
            let _: &Box<lash::persistence::StoreError> = source;
        }
    });
    variant_witness(|value: &lash::persistence::AttachmentStoreError| {
        matches!(
            value,
            lash::persistence::AttachmentStoreError::WriteRollbackFailed { .. }
        )
    });
    field_witness(|value: &lash::persistence::AttachmentStoreError| {
        if let lash::persistence::AttachmentStoreError::WriteRollbackFailed {
            attachment_id,
            write_error,
            abort_error,
        } = value
        {
            let _: &lash::attachments::AttachmentId = attachment_id;
            let _: &Box<lash::persistence::AttachmentStoreError> = write_error;
            let _: &Box<lash::persistence::StoreError> = abort_error;
        }
    });
    // W0040: lash::persistence::AttachmentStorePersistence::Ephemeral [variant]
    variant_witness(|value: &lash::persistence::AttachmentStorePersistence| {
        matches!(
            value,
            lash::persistence::AttachmentStorePersistence::Ephemeral
        )
    });
    // W0041: lash::persistence::BlobRef [struct]
    type_witness::<lash::persistence::BlobRef>();
    // W0042: lash::persistence::BlobRef::0 [field]
    field_witness(|value: &lash::persistence::BlobRef| {
        let _ = &value.0;
    });
    // W0043: lash::persistence::BlobRef::as_str [function]
    let _ = lash::persistence::BlobRef::as_str;
    // W0044: lash::persistence::GcReport::deleted_blob_count [field]
    field_witness(|value: &lash::persistence::GcReport| {
        let _ = &value.deleted_blob_count;
    });
    // W0045: lash::persistence::GcReport::retained_blob_count [field]
    field_witness(|value: &lash::persistence::GcReport| {
        let _ = &value.retained_blob_count;
    });
    // W0046: lash::persistence::RuntimeCommit::adopted_intent_rows [field]
    field_witness(|value: &lash::persistence::RuntimeCommit| {
        let _ = &value.adopted_intent_rows;
    });
    // W0047: lash::persistence::RuntimeCommit::committed_attachment_ids [field]
    field_witness(|value: &lash::persistence::RuntimeCommit| {
        let _ = &value.committed_attachment_ids;
    });
    // W0048: lash::persistence::RuntimeCommit::with_committed_attachments [function]
    let _: fn(
        lash::persistence::RuntimeCommit,
        Vec<lash::attachments::AttachmentId>,
    ) -> lash::persistence::RuntimeCommit =
        lash::persistence::RuntimeCommit::with_committed_attachments;
    // W0049: lash::persistence::RuntimeAttachmentStore [struct]
    type_witness::<lash::persistence::RuntimeAttachmentStore>();
    // W0050: lash::persistence::RuntimeAttachmentStore::backend [function]
    let _ = lash::persistence::RuntimeAttachmentStore::backend;
    // W0051: lash::persistence::RuntimeAttachmentStore::delete [function]
    let _ = lash::persistence::RuntimeAttachmentStore::delete;
    // W0052: lash::persistence::RuntimeAttachmentStore::ephemeral [function]
    let _ = lash::persistence::RuntimeAttachmentStore::ephemeral;
    // Explicit reads validate the ref's content claims.
    let _ = lash::persistence::RuntimeAttachmentStore::read;
    // W0055: lash::persistence::RuntimeAttachmentStore::referrers [function]
    let _ = lash::persistence::RuntimeAttachmentStore::referrers;
    // W0056: lash::persistence::RuntimeAttachmentStore::new [function]
    let _: fn(
        std::sync::Arc<dyn lash::persistence::AttachmentStore>,
        std::sync::Arc<dyn lash::persistence::AttachmentReferrers>,
        lash::RuntimeOwner,
    ) -> lash::persistence::RuntimeAttachmentStore = lash::persistence::RuntimeAttachmentStore::new;
    // W0057: lash::persistence::RuntimeAttachmentStore::persistence [function]
    let _ = lash::persistence::RuntimeAttachmentStore::persistence;
    // W0058: lash::persistence::RuntimeAttachmentStore::put [function]
    let _ = lash::persistence::RuntimeAttachmentStore::put;
    // W0059: lash::persistence::RuntimeAttachmentStore::holder [function]
    let _ = lash::persistence::RuntimeAttachmentStore::holder;
    // W0060: lash::persistence::StoreError::CheckpointComponentMissing::blob_ref [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointComponentMissing { blob_ref, .. } = value {
            let _ = blob_ref;
        }
    });
    // W0061: lash::persistence::StoreError::CommitByteBudgetExceeded::attachment_referrer_bytes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitByteBudgetExceeded {
            attachment_referrer_bytes,
            ..
        } = value
        {
            let _ = attachment_referrer_bytes;
        }
    });
    // W0062: lash::persistence::StoredBlobRef [struct]
    type_witness::<lash::persistence::StoredBlobRef>();
    // W0063: lash::persistence::StoredBlobRef::id [field]
    field_witness(|value: &lash::persistence::StoredBlobRef| {
        let _ = &value.id;
    });
    // W0064: lash::persistence::StoredBlobRef::last_modified_epoch_ms [field]
    field_witness(|value: &lash::persistence::StoredBlobRef| {
        let _ = &value.last_modified_epoch_ms;
    });
    // W0071: lash::tools::ToolCallOutput::attachments [function]
    let _ = lash::tools::ToolCallOutput::attachments;
    // W0073: lash::tools::AttemptContext::attachments [function]
    let _ = lash::tools::AttemptContext::attachments;
    // W0074: lash::tracing::TraceAttachment [struct]
    type_witness::<lash::tracing::TraceAttachment>();
    field_witness(|value: &lash::tracing::TraceAttachment| {
        let _: &String = &value.id;
        let _: &String = &value.media_type;
        let _: &u64 = &value.byte_len;
        let _ = &value.position;
        let _: &Option<String> = &value.delivery_form;
    });
    // W0080: lash::tracing::TraceContentBlock::Attachment [variant]
    variant_witness(|value: &lash::tracing::TraceContentBlock| {
        matches!(value, lash::tracing::TraceContentBlock::Attachment { .. })
    });
    // W0083: lash::persistence::AttachmentWrite [struct]
    type_witness::<lash::persistence::AttachmentWrite>();
    // W0084: lash::persistence::AttachmentWrite::attachment_id [field]
    field_witness(|value: &lash::persistence::AttachmentWrite| {
        let _ = &value.attachment_id;
    });
    // W0085: lash::persistence::AttachmentWrite::claim [field]
    field_witness(|value: &lash::persistence::AttachmentWrite| {
        let _ = &value.claim;
    });
    // W0090: lash::persistence::AttachmentReferrers [trait]
    fn trait_witness_0090<T: lash::persistence::AttachmentReferrers>() {}
    // W0091: lash::persistence::AttachmentReferrers::acquire_attachment_refs [function]
    fn meth_0091<T: lash::persistence::AttachmentReferrers>(_: &T) {
        let _ = T::acquire_attachment_refs;
    }
    // W0092: lash::persistence::AttachmentReferrers::forget_attachment_ref [function]
    fn meth_0092<T: lash::persistence::AttachmentReferrers>(_: &T) {
        let _ = T::forget_attachment_ref;
    }
    // W0093: lash::persistence::AttachmentReferrers::end_attachment_referrer [function]
    fn meth_0093<T: lash::persistence::AttachmentReferrers>(_: &T) {
        let _ = T::end_attachment_referrer;
    }
    // W0094: lash::persistence::AttachmentReferrers::session_referrer_state [function]
    fn meth_0094<T: lash::persistence::AttachmentReferrers>(_: &T) {
        let _ = T::session_referrer_state;
    }
    // W0096: lash::persistence::AttachmentReferrers::attachment_referrers [function]
    fn meth_0096<T: lash::persistence::AttachmentReferrers>(_: &T) {
        let _ = T::attachment_referrers;
    }
    // W0107: lash::persistence::SessionReferrerState [enum]
    type_witness::<lash::persistence::SessionReferrerState>();
    // W0108: lash::persistence::SessionReferrerState::DeletedRetained [variant]
    variant_witness(|value: &lash::persistence::SessionReferrerState| {
        matches!(
            value,
            lash::persistence::SessionReferrerState::DeletedRetained
        )
    });
    // W0109: lash::persistence::SessionReferrerState::DeletedRetired [variant]
    variant_witness(|value: &lash::persistence::SessionReferrerState| {
        matches!(
            value,
            lash::persistence::SessionReferrerState::DeletedRetired
        )
    });
    // W0111: lash::messages::PartAttachment [struct]
    type_witness::<lash::messages::PartAttachment>();
    // W0112: lash::messages::PartAttachment::reference [field]
    field_witness(|value: &lash::messages::PartAttachment| {
        let _: &lash::attachments::AttachmentRef = &value.reference;
    });
    // W0113: lash::messages::Part::attachment [field]
    field_witness(|value: &lash::messages::Part| {
        if let lash::messages::Part::Attachment { attachment, .. } = value {
            let _ = attachment;
        }
    });
    // W0114: lash::messages::Part::attachment_part [function]
    let _ = lash::messages::Part::attachment_part;
    // W0116: lash::messages::PartKind::Attachment [variant]
    variant_witness(|value: &lash::messages::PartKind| {
        matches!(value, lash::messages::PartKind::Attachment)
    });
    // W0117: lash::plugins::RuntimeExecutionContext::attachment_store [function]
    let _ = lash::plugins::RuntimeExecutionContext::attachment_store;
    // W0120: lash::persistence::AttachmentRootSet::arm_attachment_delete [function]
    fn meth_0120<T: lash::persistence::AttachmentRootSet>(_: &T) {
        let _ = T::arm_attachment_delete;
    }
    // W0121: lash::persistence::AttachmentRootSet::condemn_attachment [function]
    fn meth_0121<T: lash::persistence::AttachmentRootSet>(_: &T) {
        let _ = T::condemn_attachment;
    }
    // W0122: lash::persistence::AttachmentRootSet::fence [function]
    fn meth_0122<T: lash::persistence::AttachmentRootSet>(_: &T) {
        let _ = T::fence;
    }
    // W0124: lash::persistence::AttachmentCondemnation [enum]
    type_witness::<lash::persistence::AttachmentCondemnation>();
    // W0125: lash::persistence::AttachmentCondemnation::AlreadyCondemned [variant]
    variant_witness(|value: &lash::persistence::AttachmentCondemnation| {
        matches!(
            value,
            lash::persistence::AttachmentCondemnation::AlreadyCondemned
        )
    });
    // W0126: lash::persistence::AttachmentCondemnation::Condemned [variant]
    variant_witness(|value: &lash::persistence::AttachmentCondemnation| {
        matches!(value, lash::persistence::AttachmentCondemnation::Condemned)
    });
    // W0127: lash::persistence::AttachmentCondemnation::RootPresent [variant]
    variant_witness(|value: &lash::persistence::AttachmentCondemnation| {
        matches!(
            value,
            lash::persistence::AttachmentCondemnation::RootPresent
        )
    });
    // W0128: lash::persistence::AttachmentCondemnation::Unsupported [variant]
    variant_witness(|value: &lash::persistence::AttachmentCondemnation| {
        matches!(
            value,
            lash::persistence::AttachmentCondemnation::Unsupported
        )
    });
    // W0129: lash::persistence::AttachmentDeleteArming [enum]
    type_witness::<lash::persistence::AttachmentDeleteArming>();
    // W0130: lash::persistence::AttachmentDeleteArming::Armed [variant]
    variant_witness(|value: &lash::persistence::AttachmentDeleteArming| {
        matches!(value, lash::persistence::AttachmentDeleteArming::Armed)
    });
    // W0131: lash::persistence::AttachmentDeleteArming::Revoked [variant]
    variant_witness(|value: &lash::persistence::AttachmentDeleteArming| {
        matches!(value, lash::persistence::AttachmentDeleteArming::Revoked)
    });
    // W0132: lash::persistence::AttachmentReferrers::begin_attachment_write [function]
    fn meth_0132<T: lash::persistence::AttachmentReferrers>(_: &T) {
        let _ = T::begin_attachment_write;
    }
    // W0133: lash::persistence::AttachmentWriteFence [enum]
    type_witness::<lash::persistence::AttachmentWriteFence>();
    // W0134: lash::persistence::AttachmentWriteFence::Granted [variant]
    variant_witness(|value: &lash::persistence::AttachmentWriteFence| {
        matches!(value, lash::persistence::AttachmentWriteFence::Granted(..))
    });
    // W0135: lash::persistence::AttachmentWriteFence::ReclamationInFlight [variant]
    variant_witness(|value: &lash::persistence::AttachmentWriteFence| {
        matches!(
            value,
            lash::persistence::AttachmentWriteFence::ReclamationInFlight
        )
    });
    // W0136: lash::persistence::AttachmentReclamationFailure [type_alias]
    type_witness::<lash::persistence::AttachmentReclamationFailure>();
} // W0120: lash_core::impl_noop_attachment_referrers [macro]
struct NoopManifestWitness;
lash_core::impl_noop_attachment_referrers!(NoopManifestWitness);

// Host uploads finish before an input can name the ref. The operation retains
// typed store failures and uses the session's guarded upload holder.
async fn host_upload_is_ref_only(
    session: &lash::LashSession,
    bytes: Vec<u8>,
    meta: lash::attachments::AttachmentCreateMeta,
) -> Result<lash::InputItem, lash::persistence::AttachmentStoreError> {
    let reference: lash::attachments::AttachmentRef = session.put_attachment(bytes, meta).await?;
    Ok(lash::InputItem::attachment(reference))
}

fn host_store_delivery_contracts() {
    type_witness::<lash::attachments::AttachmentPosition>();
    type_witness::<lash::attachments::DeliveryForms>();
    type_witness::<lash::attachments::ProviderAccepts>();
    type_witness::<lash::attachments::ProviderFileScope>();
    type_witness::<lash::attachments::Delivery>();
    type_witness::<lash::attachments::DeliverySecret>();
    type_witness::<lash::attachments::DeliveryLimits>();
    type_witness::<lash::attachments::DeliveryContext>();
    type_witness::<lash::persistence::ContentMismatchDetail>();
    type_witness::<lash::persistence::ProviderFileDelivery>();
    type_witness::<lash::persistence::ProviderFileCacheLimits>();
    type_witness::<lash::persistence::UploadedProviderFile>();
    type_witness::<dyn lash::persistence::ProviderFileUploader>();
    type_witness::<lash::provider::AttachmentDeliveryError>();
    type_witness::<lash::provider::NoSlotDeliveries>();
    type_witness::<dyn lash::provider::SlotDeliveries>();
    let _ = lash::persistence::ProviderFileDelivery::new;
    let _ = lash::LashCoreBuilder::provider_file_uploaders;
    let _ = lash::LashCoreBuilder::provider_file_cache;
    fn store_methods<T: lash::persistence::AttachmentStore>() {
        let _ = T::deliver;
        let _ = T::invalidate_delivery;
    }
}
