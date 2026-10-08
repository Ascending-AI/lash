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
        "image/gif",
        "image/webp",
        "application/pdf",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let rules_0 = vec![AttachmentAcceptanceRule {
        positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
        media_types: types_0.clone(),
        media_families: [].into_iter().map(str::to_owned).collect(),
        forms: DeliveryForms {
            bytes: true,
            url: true,
            provider_file: true,
        },
    }];
    std::sync::Arc::new(AttachmentCapabilitySnapshot {
        revision: "test-host-revision-1".into(),
        acceptors: vec![AttachmentAcceptor {
            provider: "anthropic".into(),
            rules: rules_0,
        }],
    })
}
