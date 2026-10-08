use lash_sansio::llm::attachment_delivery::{AttachmentPosition, DeliveryForms};
#[cfg(test)]
pub(crate) fn attachment_test_acceptance()
-> std::sync::Arc<lash_core::provider::AttachmentCapabilitySnapshot> {
    use lash_core::provider::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
    };
    let types_0 = [
        "image/jpeg",
        "image/png",
        "image/webp",
        "image/heic",
        "image/heif",
        "application/pdf",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let rules_0 = vec![AttachmentAcceptanceRule {
        positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
        media_types: types_0.clone(),
        media_families: ["image", "audio", "text", "video"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        forms: DeliveryForms {
            bytes: true,
            url: false,
            provider_file: true,
        },
    }];
    std::sync::Arc::new(AttachmentCapabilitySnapshot {
        revision: "test-host-revision-1".into(),
        acceptors: vec![AttachmentAcceptor {
            provider: "google_oauth".into(),
            rules: rules_0,
        }],
    })
}
