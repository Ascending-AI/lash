//! Compile-time witnesses for attachment-area facade and integrator contracts.
//!
//! FIG-2107 drains the ledger's remaining `unused-justify` slices: at the
//! dispatch-time recount this area held 136 rows. The 120 rows whose item
//! still exists are type-checked here through the path a host or integrator
//! would name — `lash::` for facade surface, `lash_core::` for internal seams
//! the integrator classes consume directly. The 16 rows whose item no longer
//! exists anywhere in this workspace are listed in the pull request rather
//! than witnessed here.

#![cfg(feature = "testing")]
#![allow(dead_code, unreachable_code, unused_variables, unused_imports)]
#![allow(clippy::all)]

fn type_witness<T>() {}
fn member_witness<T>(_: T) {}
fn field_witness<T>(_: impl FnOnce(&T)) {}
fn variant_witness<T>(_: impl FnOnce(&T) -> bool) {}

fn drain_area_witnesses() {
    // W0001: lash::InputItem::Attachment [variant]
    variant_witness(|value: &lash::InputItem| matches!(value, lash::InputItem::Attachment { .. }));
    // W0002: lash::InputItem::Attachment::source [field]
    field_witness(|value: &lash::InputItem| {
        if let lash::InputItem::Attachment { source, .. } = value {
            let _ = source;
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
    // W0006: lash::direct::AttachmentSource::ExternalUrl [variant]
    variant_witness(|value: &lash::direct::AttachmentSource| {
        matches!(value, lash::direct::AttachmentSource::ExternalUrl { .. })
    });
    // W0007: lash::direct::AttachmentSource::ExternalUrl::media_type [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::ExternalUrl { media_type, .. } = value {
            let _ = media_type;
        }
    });
    // W0008: lash::direct::AttachmentSource::ExternalUrl::url [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::ExternalUrl { url, .. } = value {
            let _ = url;
        }
    });
    // W0009: lash::direct::AttachmentSource::Inline [variant]
    variant_witness(|value: &lash::direct::AttachmentSource| {
        matches!(value, lash::direct::AttachmentSource::Inline { .. })
    });
    // W0010: lash::direct::AttachmentSource::Inline::bytes [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::Inline { bytes, .. } = value {
            let _ = bytes;
        }
    });
    // W0011: lash::direct::AttachmentSource::Inline::media_type [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::Inline { media_type, .. } = value {
            let _ = media_type;
        }
    });
    // W0012: lash::direct::AttachmentSource::ProviderFile [variant]
    variant_witness(|value: &lash::direct::AttachmentSource| {
        matches!(value, lash::direct::AttachmentSource::ProviderFile { .. })
    });
    // W0013: lash::direct::AttachmentSource::ProviderFile::id [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::ProviderFile { id, .. } = value {
            let _ = id;
        }
    });
    // W0014: lash::direct::AttachmentSource::ProviderFile::media_type [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::ProviderFile { media_type, .. } = value {
            let _ = media_type;
        }
    });
    // W0015: lash::direct::AttachmentSource::ProviderFile::provider_scope [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::ProviderFile { provider_scope, .. } = value {
            let _ = provider_scope;
        }
    });
    // W0016: lash::direct::AttachmentSource::Stored::attachment_ref [field]
    field_witness(|value: &lash::direct::AttachmentSource| {
        if let lash::direct::AttachmentSource::Stored { attachment_ref, .. } = value {
            let _ = attachment_ref;
        }
    });
    // W0017: lash::direct::AttachmentSource::external_url [function]
    let _: fn(lash::attachments::MediaType, String) -> lash::direct::AttachmentSource =
        lash::direct::AttachmentSource::external_url;
    // W0018: lash::direct::AttachmentSource::inline [function]
    let _ = lash::direct::AttachmentSource::inline;
    // W0019: lash::direct::AttachmentSource::provider_file [function]
    let _: fn(
        lash::direct::ProviderFileScope,
        String,
        Option<lash::attachments::MediaType>,
    ) -> lash::direct::AttachmentSource = lash::direct::AttachmentSource::provider_file;
    // W0020: lash::direct::DirectPart::Attachment [variant]
    variant_witness(|value: &lash::direct::DirectPart| {
        matches!(value, lash::direct::DirectPart::Attachment(..))
    });
    // W0021: lash::direct::DirectPart::Attachment::0 [field]
    field_witness(|value: &lash::direct::DirectPart| {
        if let lash::direct::DirectPart::Attachment(f0) = value {
            let _ = f0;
        }
    });
    // W0023: lash::durability::RuntimeHostConfig::attachment_source_policy [field]
    field_witness(|value: &lash::durability::RuntimeHostConfig| {
        let _ = &value.attachment_source_policy;
    });
    // W0024: lash::durability::RuntimeHostConfig::with_attachment_source_policy [function]
    let _ = lash::durability::RuntimeHostConfig::with_attachment_source_policy;
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
    // W0038: lash::persistence::AttachmentStoreError::ManifestRecordFailed [variant]
    variant_witness(|value: &lash::persistence::AttachmentStoreError| {
        matches!(
            value,
            lash::persistence::AttachmentStoreError::ManifestRecordFailed(..)
        )
    });
    // W0039: lash::persistence::AttachmentStoreError::ManifestRecordFailed::0 [field]
    field_witness(|value: &lash::persistence::AttachmentStoreError| {
        if let lash::persistence::AttachmentStoreError::ManifestRecordFailed(f0) = value {
            let _ = f0;
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
    // W0049: lash::persistence::SessionAttachmentStore [struct]
    type_witness::<lash::persistence::SessionAttachmentStore>();
    // W0050: lash::persistence::SessionAttachmentStore::backend [function]
    let _ = lash::persistence::SessionAttachmentStore::backend;
    // W0051: lash::persistence::SessionAttachmentStore::delete [function]
    let _ = lash::persistence::SessionAttachmentStore::delete;
    // W0052: lash::persistence::SessionAttachmentStore::ephemeral [function]
    let _ = lash::persistence::SessionAttachmentStore::ephemeral;
    // W0053: lash::persistence::SessionAttachmentStore::get [function]
    let _ = lash::persistence::SessionAttachmentStore::get;
    // W0054: lash::persistence::SessionAttachmentStore::in_memory [function]
    let _ = lash::persistence::SessionAttachmentStore::in_memory;
    // W0055: lash::persistence::SessionAttachmentStore::manifest [function]
    let _ = lash::persistence::SessionAttachmentStore::manifest;
    // W0056: lash::persistence::SessionAttachmentStore::new [function]
    let _: fn(
        std::sync::Arc<dyn lash::persistence::AttachmentStore>,
        std::sync::Arc<dyn lash::persistence::AttachmentManifest>,
        String,
    ) -> lash::persistence::SessionAttachmentStore = lash::persistence::SessionAttachmentStore::new;
    // W0057: lash::persistence::SessionAttachmentStore::persistence [function]
    let _ = lash::persistence::SessionAttachmentStore::persistence;
    // W0058: lash::persistence::SessionAttachmentStore::put [function]
    let _ = lash::persistence::SessionAttachmentStore::put;
    // W0059: lash::persistence::SessionAttachmentStore::session_id [function]
    let _ = lash::persistence::SessionAttachmentStore::session_id;
    // W0060: lash::persistence::StoreError::CheckpointComponentMissing::blob_ref [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CheckpointComponentMissing { blob_ref, .. } = value {
            let _ = blob_ref;
        }
    });
    // W0061: lash::persistence::StoreError::CommitByteBudgetExceeded::attachment_manifest_bytes [field]
    field_witness(|value: &lash::persistence::StoreError| {
        if let lash::persistence::StoreError::CommitByteBudgetExceeded {
            attachment_manifest_bytes,
            ..
        } = value
        {
            let _ = attachment_manifest_bytes;
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
    // W0072: lash::tools::ToolCallOutput::replace_attachment_source [function]
    let _ = lash::tools::ToolCallOutput::replace_attachment_source;
    // W0073: lash::tools::ToolContext::attachments [function]
    let _ = lash::tools::ToolContext::attachments;
    // W0074: lash::tracing::TraceAttachment [struct]
    type_witness::<lash::tracing::TraceAttachment>();
    // W0075: lash::tracing::TraceAttachment::bytes_len [field]
    field_witness(|value: &lash::tracing::TraceAttachment| {
        let _ = &value.bytes_len;
    });
    // W0076: lash::tracing::TraceAttachment::bytes_sha256 [field]
    field_witness(|value: &lash::tracing::TraceAttachment| {
        let _ = &value.bytes_sha256;
    });
    // W0077: lash::tracing::TraceAttachment::filename [field]
    field_witness(|value: &lash::tracing::TraceAttachment| {
        let _ = &value.filename;
    });
    // W0078: lash::tracing::TraceAttachment::mime [field]
    field_witness(|value: &lash::tracing::TraceAttachment| {
        let _ = &value.mime;
    });
    // W0079: lash::tracing::TraceAttachment::source [field]
    field_witness(|value: &lash::tracing::TraceAttachment| {
        let _ = &value.source;
    });
    // W0080: lash::tracing::TraceContentBlock::Attachment [variant]
    variant_witness(|value: &lash::tracing::TraceContentBlock| {
        matches!(value, lash::tracing::TraceContentBlock::Attachment { .. })
    });
    // W0083: lash::persistence::AttachmentIntent [struct]
    type_witness::<lash::persistence::AttachmentIntent>();
    // W0084: lash::persistence::AttachmentIntent::attachment_id [field]
    field_witness(|value: &lash::persistence::AttachmentIntent| {
        let _ = &value.attachment_id;
    });
    // W0085: lash::persistence::AttachmentIntent::canonical_uri [field]
    field_witness(|value: &lash::persistence::AttachmentIntent| {
        let _ = &value.canonical_uri;
    });
    // W0086: lash::persistence::AttachmentIntent::intent_at_epoch_ms [field]
    field_witness(|value: &lash::persistence::AttachmentIntent| {
        let _ = &value.intent_at_epoch_ms;
    });
    // W0089: lash::persistence::AttachmentIntent::session_id [field]
    field_witness(|value: &lash::persistence::AttachmentIntent| {
        let _ = &value.session_id;
    });
    // W0090: lash::persistence::AttachmentManifest [trait]
    fn trait_witness_0090<T: lash::persistence::AttachmentManifest>() {}
    // W0091: lash::persistence::AttachmentManifest::commit_refs [function]
    fn meth_0091<T: lash::persistence::AttachmentManifest>(_: &T) {
        let _ = T::commit_refs;
    }
    // W0092: lash::persistence::AttachmentManifest::forget [function]
    fn meth_0092<T: lash::persistence::AttachmentManifest>(_: &T) {
        let _ = T::forget;
    }
    // W0093: lash::persistence::AttachmentManifest::forget_aged_uncommitted_intents [function]
    fn meth_0093<T: lash::persistence::AttachmentManifest>(_: &T) {
        let _ = T::forget_aged_uncommitted_intents;
    }
    // W0094: lash::persistence::AttachmentManifest::has_live_ref_for_id [function]
    fn meth_0094<T: lash::persistence::AttachmentManifest>(_: &T) {
        let _ = T::has_live_ref_for_id;
    }
    // W0096: lash::persistence::AttachmentManifest::list_all_refs [function]
    fn meth_0096<T: lash::persistence::AttachmentManifest>(_: &T) {
        let _ = T::list_all_refs;
    }
    // W0097: lash::persistence::AttachmentManifest::list_uncommitted [function]
    fn meth_0097<T: lash::persistence::AttachmentManifest>(_: &T) {
        let _ = T::list_uncommitted;
    }
    // W0099: lash::persistence::AttachmentManifestEntry [struct]
    type_witness::<lash::persistence::AttachmentManifestEntry>();
    // W0100: lash::persistence::AttachmentManifestEntry::attachment_id [field]
    field_witness(|value: &lash::persistence::AttachmentManifestEntry| {
        let _ = &value.attachment_id;
    });
    // W0101: lash::persistence::AttachmentManifestEntry::canonical_uri [field]
    field_witness(|value: &lash::persistence::AttachmentManifestEntry| {
        let _ = &value.canonical_uri;
    });
    // W0102: lash::persistence::AttachmentManifestEntry::committed_at_epoch_ms [field]
    field_witness(|value: &lash::persistence::AttachmentManifestEntry| {
        let _ = &value.committed_at_epoch_ms;
    });
    // W0103: lash::persistence::AttachmentManifestEntry::intent_at_epoch_ms [field]
    field_witness(|value: &lash::persistence::AttachmentManifestEntry| {
        let _ = &value.intent_at_epoch_ms;
    });
    // W0106: lash::persistence::AttachmentManifestEntry::session_id [field]
    field_witness(|value: &lash::persistence::AttachmentManifestEntry| {
        let _ = &value.session_id;
    });
    // W0107: lash::persistence::AttachmentOwnerKind [enum]
    type_witness::<lash::persistence::AttachmentOwnerKind>();
    // W0108: lash::persistence::AttachmentOwnerKind::Process [variant]
    variant_witness(|value: &lash::persistence::AttachmentOwnerKind| {
        matches!(value, lash::persistence::AttachmentOwnerKind::Process)
    });
    // W0109: lash::persistence::AttachmentOwnerKind::Turn [variant]
    variant_witness(|value: &lash::persistence::AttachmentOwnerKind| {
        matches!(value, lash::persistence::AttachmentOwnerKind::Turn)
    });
    // W0110: lash::persistence::AttachmentOwnerKind::as_str [function]
    let _ = lash::persistence::AttachmentOwnerKind::as_str;
    // W0111: lash::messages::PartAttachment [struct]
    type_witness::<lash::messages::PartAttachment>();
    // W0112: lash::messages::PartAttachment::source [field]
    field_witness(|value: &lash::messages::PartAttachment| {
        let _ = &value.source;
    });
    // W0113: lash::messages::Part::attachment [field]
    field_witness(|value: &lash::messages::Part| {
        if let lash::messages::Part::Attachment { attachment, .. } = value {
            let _ = attachment;
        }
    });
    // W0114: lash::messages::Part::attachment_part [function]
    let _ = lash::messages::Part::attachment_part;
    // W0115: lash::messages::Part::tool_result_attachment [function]
    let _ = lash::messages::Part::tool_result_attachment;
    // W0116: lash::messages::PartKind::Attachment [variant]
    variant_witness(|value: &lash::messages::PartKind| {
        matches!(value, lash::messages::PartKind::Attachment)
    });
    // W0117: lash::plugins::RuntimeExecutionContext::attachment_store [function]
    let _ = lash::plugins::RuntimeExecutionContext::attachment_store;
    // W0118: lash_core::test_support::AttachmentProducer::Tool::tool_name [field]
    field_witness(|value: &lash_core::test_support::AttachmentProducer| {
        if let lash_core::test_support::AttachmentProducer::Tool { tool_name, .. } = value {
            let _ = tool_name;
        }
    });
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
    // W0123: lash::persistence::AttachmentRootSet::release_attachment_condemnation [function]
    fn meth_0123<T: lash::persistence::AttachmentRootSet>(_: &T) {
        let _ = T::release_attachment_condemnation;
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
    // W0132: lash::persistence::AttachmentManifest::begin_attachment_write [function]
    fn meth_0132<T: lash::persistence::AttachmentManifest>(_: &T) {
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
} // W0120: lash_core::impl_noop_attachment_manifest [macro]
struct NoopManifestWitness;
lash_core::impl_noop_attachment_manifest!(NoopManifestWitness);
