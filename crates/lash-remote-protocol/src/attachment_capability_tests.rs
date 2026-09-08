use super::*;

#[test]
fn attachment_acceptance_wire_literals_are_pinned() {
    for (source, expected) in [
        (RemoteAttachmentMimeSource::Inline, "inline"),
        (RemoteAttachmentMimeSource::Stored, "stored"),
        (RemoteAttachmentMimeSource::ExternalUrl, "external_url"),
    ] {
        assert_eq!(
            serde_json::to_value(source).unwrap(),
            serde_json::json!(expected)
        );
    }
    let snapshot = RemoteAttachmentCapabilitySnapshot {
        revision: "host-42".to_string(),
        acceptors: vec![RemoteAttachmentAcceptor {
            provider: "fixture".to_string(),
            rules: vec![
                RemoteAttachmentAcceptanceRule::Mime {
                    source: RemoteAttachmentMimeSource::Stored,
                    media_types: vec!["image/png".to_string()],
                    media_families: Vec::new(),
                },
                RemoteAttachmentAcceptanceRule::ProviderFile {
                    provider: "fixture".to_string(),
                },
            ],
        }],
    };
    assert_eq!(
        serde_json::to_value(&snapshot).unwrap(),
        serde_json::json!({
            "revision": "host-42", "acceptors": [{"provider": "fixture", "rules": [
                {"kind": "mime", "source": "stored", "media_types": ["image/png"], "media_families": []},
                {"kind": "provider_file", "provider": "fixture"}
            ]}]
        })
    );
    #[cfg(feature = "core-conversions")]
    {
        let core: lash_core::provider::AttachmentCapabilitySnapshot = snapshot.clone().into();
        assert_eq!(RemoteAttachmentCapabilitySnapshot::from(core), snapshot);
    }
    let capability = RemoteModelCapability {
        attachment_acceptance: snapshot.clone(),
        ..Default::default()
    };
    assert!(!capability.is_empty());
    #[cfg(feature = "core-conversions")]
    assert!(!lash_core::provider::ModelCapability::from(capability.clone()).is_empty());
    let intent = RemoteModelIntent {
        model: "fixture".into(),
        variant: Default::default(),
        capability,
        provider: None,
        metadata: Default::default(),
    };
    assert_eq!(
        serde_json::to_value(intent).unwrap()["capability"]["attachment_acceptance"],
        serde_json::to_value(snapshot).unwrap()
    );
}
