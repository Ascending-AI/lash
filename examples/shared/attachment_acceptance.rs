//! Host model acceptance, intersected with the serving adapter at lowering.
use lash::attachments::{AttachmentPosition, DeliveryForms};
use lash::provider::{AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot};
pub(crate) fn snapshot(
    revision: &str,
    forms: DeliveryForms,
    media_types: &[&str],
) -> AttachmentCapabilitySnapshot {
    AttachmentCapabilitySnapshot {
        revision: revision.into(),
        acceptors: vec![AttachmentAcceptor {
            provider: "openai-compatible".into(),
            rules: vec![AttachmentAcceptanceRule {
                positions: vec![AttachmentPosition::Message],
                media_types: media_types.iter().map(|m| (*m).to_owned()).collect(),
                media_families: Vec::new(),
                forms,
            }],
        }],
    }
}
