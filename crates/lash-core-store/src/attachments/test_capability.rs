use lash_sansio::llm::attachment_delivery::{AttachmentPosition, DeliveryForms};
#[cfg(any(test, feature = "testing"))]
pub fn attachment_test_acceptance() -> std::sync::Arc<crate::provider::AttachmentCapabilitySnapshot>
{
    use crate::provider::{
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
    let types_2 = [
        "image/jpeg",
        "image/png",
        "image/gif",
        "image/webp",
        "application/pdf",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let rules_2 = vec![AttachmentAcceptanceRule {
        positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
        media_types: types_2.clone(),
        media_families: [].into_iter().map(str::to_owned).collect(),
        forms: DeliveryForms {
            bytes: true,
            url: true,
            provider_file: true,
        },
    }];
    let types_3 = [
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
    let rules_3 = vec![AttachmentAcceptanceRule {
        positions: vec![AttachmentPosition::Message, AttachmentPosition::ToolResult],
        media_types: types_3.clone(),
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
        acceptors: vec![
            AttachmentAcceptor {
                provider: "sim".into(),
                rules: rules_0.clone(),
            },
            AttachmentAcceptor {
                provider: "test".into(),
                rules: rules_0.clone(),
            },
            AttachmentAcceptor {
                provider: "openai".into(),
                rules: rules_0,
            },
            AttachmentAcceptor {
                provider: "openai-compatible".into(),
                rules: rules_1,
            },
            AttachmentAcceptor {
                provider: "anthropic".into(),
                rules: rules_2,
            },
            AttachmentAcceptor {
                provider: "google_oauth".into(),
                rules: rules_3,
            },
        ],
    })
}
