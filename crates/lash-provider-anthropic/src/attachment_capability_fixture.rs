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
        provider: "anthropic".into(),
    }])
    .collect();
    ModelCapability {
        attachment_acceptance: AttachmentCapabilitySnapshot {
            revision: "test-host-revision-1".into(),
            acceptors: vec![AttachmentAcceptor {
                provider: "Anthropic Messages".into(),
                rules: rules_0,
            }],
        }
        .into(),
        ..Default::default()
    }
}
