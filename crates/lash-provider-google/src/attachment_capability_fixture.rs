#[cfg(test)]
pub(crate) fn attachment_test_acceptance()
-> std::sync::Arc<lash_core::provider::AttachmentCapabilitySnapshot> {
    use lash_core::provider::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
        AttachmentMimeSource,
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
    let rules_0 = [AttachmentMimeSource::Inline, AttachmentMimeSource::Stored]
        .into_iter()
        .map(|source| AttachmentAcceptanceRule::Mime {
            source,
            media_types: types_0.clone(),
            media_families: ["audio", "text", "video"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        })
        .chain([
            AttachmentAcceptanceRule::ProviderFile {
                provider: "google".into(),
            },
            AttachmentAcceptanceRule::ProviderFile {
                provider: "google_oauth".into(),
            },
            AttachmentAcceptanceRule::ProviderFile {
                provider: "gemini".into(),
            },
        ])
        .collect();
    std::sync::Arc::new(AttachmentCapabilitySnapshot {
        revision: "test-host-revision-1".into(),
        acceptors: vec![AttachmentAcceptor {
            provider: "Google Gemini".into(),
            rules: rules_0,
        }],
    })
}
