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
        "application/json",
        "application/msword",
        "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "application/vnd.ms-excel",
        "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "application/vnd.ms-powerpoint",
        "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "text/csv",
        "text/html",
        "text/markdown",
        "text/plain",
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
    let types_1 = ["image/jpeg", "image/png", "image/gif", "image/webp"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let rules_1 = vec![AttachmentAcceptanceRule {
        positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
        media_types: types_1.clone(),
        media_families: [].into_iter().map(str::to_owned).collect(),
        forms: DeliveryForms {
            bytes: true,
            url: true,
            provider_file: false,
        },
    }];
    std::sync::Arc::new(AttachmentCapabilitySnapshot {
        revision: "test-host-revision-1".into(),
        acceptors: vec![
            AttachmentAcceptor {
                provider: "openai".into(),
                rules: rules_0.clone(),
            },
            AttachmentAcceptor {
                provider: "codex".into(),
                rules: rules_0,
            },
            AttachmentAcceptor {
                provider: "openai-compatible".into(),
                rules: rules_1,
            },
        ],
    })
}
