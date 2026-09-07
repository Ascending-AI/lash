#[cfg(test)]
pub(crate) fn attachment_test_capability() -> lash_core::provider::ModelCapability {
    use lash_core::provider::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
        AttachmentMimeSource, ModelCapability,
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
    let rules_0 = [
        AttachmentMimeSource::Inline,
        AttachmentMimeSource::Stored,
        AttachmentMimeSource::ExternalUrl,
    ]
    .into_iter()
    .map(|source| AttachmentAcceptanceRule::Mime {
        source,
        media_types: types_0.clone(),
        media_families: [].into_iter().map(str::to_owned).collect(),
    })
    .chain([AttachmentAcceptanceRule::ProviderFile {
        provider: "openai".into(),
    }])
    .collect();
    let types_1 = ["image/jpeg", "image/png", "image/gif", "image/webp"]
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let rules_1 = [
        AttachmentMimeSource::Inline,
        AttachmentMimeSource::Stored,
        AttachmentMimeSource::ExternalUrl,
    ]
    .into_iter()
    .map(|source| AttachmentAcceptanceRule::Mime {
        source,
        media_types: types_1.clone(),
        media_families: [].into_iter().map(str::to_owned).collect(),
    })
    .collect();
    ModelCapability {
        attachment_acceptance: AttachmentCapabilitySnapshot {
            revision: "test-host-revision-1".into(),
            acceptors: vec![
                AttachmentAcceptor {
                    provider: "OpenAI Responses".into(),
                    rules: rules_0,
                },
                AttachmentAcceptor {
                    provider: "OpenAI Chat Completions".into(),
                    rules: rules_1,
                },
            ],
        }
        .into(),
        ..Default::default()
    }
}
