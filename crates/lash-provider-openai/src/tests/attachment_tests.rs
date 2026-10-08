use super::*;
use lash_core::llm::types::{AttachmentSlot, SlotCodec};
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliverySecret, ProviderFileScope,
};

// DELIVERY-SCOPE: Chat has no Files namespace, even when the host permits one.
#[test]
fn chat_never_accepts_provider_files() {
    let provider = OpenAiCompatibleProvider::new("key", "https://example.invalid/v1");
    let mime = lash_sansio::MediaType::parse("image/png").unwrap();
    let scope = ProviderFileScope {
        provider: provider.kind().into(),
        endpoint: provider.route_identity("").endpoint.into(),
        credential_scope: "account".into(),
    };
    for position in [AttachmentPosition::Message, AttachmentPosition::ToolResult] {
        let accepts = provider.attachment_accepts("model", &mime, position);
        assert!(accepts.provider_file.is_none());
        if position == AttachmentPosition::ToolResult {
            assert!(accepts.is_empty());
        }
        let reference = lash_sansio::AttachmentRef {
            id: lash_sansio::AttachmentId::parse("a".repeat(64)).unwrap(),
            media_type: mime.clone(),
            byte_len: 1,
            label: None,
            type_metadata: None,
        };
        // A forged pinned acceptance cannot make the Chat codec encode a file id.
        let slot = AttachmentSlot {
            reference,
            position,
            accepts: lash_sansio::llm::attachment_delivery::ProviderAccepts {
                bytes: true,
                url: true,
                provider_file: Some(scope.clone()),
            },
            codec: SlotCodec {
                name: crate::attachment_delivery::CHAT_CODEC.into(),
                revision: 1,
            },
        };
        let delivered = Delivery::ProviderFile {
            scope: scope.clone(),
            id: DeliverySecret::new("file-secret".into()),
            valid_until_ms: None,
        };
        let error = provider.encode_slot(&slot, &delivered).unwrap_err();
        assert_eq!(error.retry_verdict, TransportRetryVerdict::Forbidden);
        assert_eq!(
            error.code.as_ref().unwrap().spelling(),
            "admitted_request_unavailable"
        );
        assert!(!format!("{error:?}").contains("file-secret"));
    }
}
