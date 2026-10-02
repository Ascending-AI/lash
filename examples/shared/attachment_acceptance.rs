//! The attachment-acceptance snapshot the example hosts record on their
//! sessions.
//!
//! `SessionSpec::attachment_acceptance` records which attachment sources and
//! media types the host's model catalogue admits. Each example host owns its
//! catalogue and its `revision`; an existing session retains the snapshot it
//! recorded at its creation when the catalogue changes, so a host bumps the
//! revision when its admittance policy moves.

use lash::provider::{
    AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
    AttachmentMimeSource,
};

/// One `Mime` rule per entry in `sources`, each admitting `media_types`,
/// under the one provider label the examples' OpenAI-compatible transports
/// answer to, recorded as catalogue `revision`.
pub(crate) fn snapshot(
    revision: &str,
    sources: &[AttachmentMimeSource],
    media_types: &[&str],
) -> AttachmentCapabilitySnapshot {
    AttachmentCapabilitySnapshot {
        revision: revision.into(),
        acceptors: [AttachmentAcceptor {
            provider: "OpenAI Chat Completions".into(),
            rules: sources
                .iter()
                .map(|&source| AttachmentAcceptanceRule::Mime {
                    source,
                    media_types: media_types
                        .iter()
                        .map(|media_type| media_type.to_string())
                        .collect(),
                    media_families: Vec::new(),
                })
                .collect(),
        }]
        .into_iter()
        .collect(),
    }
}
