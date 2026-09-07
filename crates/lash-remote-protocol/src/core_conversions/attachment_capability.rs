use super::*;
impl From<core_llm::AttachmentCapabilitySnapshot> for RemoteAttachmentCapabilitySnapshot {
    fn from(value: core_llm::AttachmentCapabilitySnapshot) -> Self {
        let core_llm::AttachmentCapabilitySnapshot {
            revision,
            acceptors,
        } = value;
        Self {
            revision,
            acceptors: acceptors.into_iter().map(Into::into).collect(),
        }
    }
}
impl From<RemoteAttachmentCapabilitySnapshot> for core_llm::AttachmentCapabilitySnapshot {
    fn from(value: RemoteAttachmentCapabilitySnapshot) -> Self {
        let RemoteAttachmentCapabilitySnapshot {
            revision,
            acceptors,
        } = value;
        Self {
            revision,
            acceptors: acceptors.into_iter().map(Into::into).collect(),
        }
    }
}
impl From<core_llm::AttachmentAcceptor> for RemoteAttachmentAcceptor {
    fn from(value: core_llm::AttachmentAcceptor) -> Self {
        let core_llm::AttachmentAcceptor { provider, rules } = value;
        Self {
            provider,
            rules: rules.into_iter().map(Into::into).collect(),
        }
    }
}
impl From<RemoteAttachmentAcceptor> for core_llm::AttachmentAcceptor {
    fn from(value: RemoteAttachmentAcceptor) -> Self {
        let RemoteAttachmentAcceptor { provider, rules } = value;
        Self {
            provider,
            rules: rules.into_iter().map(Into::into).collect(),
        }
    }
}
impl From<core_llm::AttachmentAcceptanceRule> for RemoteAttachmentAcceptanceRule {
    fn from(value: core_llm::AttachmentAcceptanceRule) -> Self {
        match value {
            core_llm::AttachmentAcceptanceRule::Mime {
                source,
                media_types,
                media_families,
            } => Self::Mime {
                source: source.into(),
                media_types,
                media_families,
            },
            core_llm::AttachmentAcceptanceRule::ProviderFile { provider } => {
                Self::ProviderFile { provider }
            }
        }
    }
}
impl From<RemoteAttachmentAcceptanceRule> for core_llm::AttachmentAcceptanceRule {
    fn from(value: RemoteAttachmentAcceptanceRule) -> Self {
        match value {
            RemoteAttachmentAcceptanceRule::Mime {
                source,
                media_types,
                media_families,
            } => Self::Mime {
                source: source.into(),
                media_types,
                media_families,
            },
            RemoteAttachmentAcceptanceRule::ProviderFile { provider } => {
                Self::ProviderFile { provider }
            }
        }
    }
}
impl From<core_llm::AttachmentMimeSource> for RemoteAttachmentMimeSource {
    fn from(value: core_llm::AttachmentMimeSource) -> Self {
        match value {
            core_llm::AttachmentMimeSource::Inline => Self::Inline,
            core_llm::AttachmentMimeSource::Stored => Self::Stored,
            core_llm::AttachmentMimeSource::ExternalUrl => Self::ExternalUrl,
        }
    }
}
impl From<RemoteAttachmentMimeSource> for core_llm::AttachmentMimeSource {
    fn from(value: RemoteAttachmentMimeSource) -> Self {
        match value {
            RemoteAttachmentMimeSource::Inline => Self::Inline,
            RemoteAttachmentMimeSource::Stored => Self::Stored,
            RemoteAttachmentMimeSource::ExternalUrl => Self::ExternalUrl,
        }
    }
}
