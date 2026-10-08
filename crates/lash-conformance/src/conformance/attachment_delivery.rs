//! ATTACHMENT-IDENTITY: delivery verifies the immutable original's claims.
use crate::*;
use lash_sansio::llm::attachment_delivery::{Delivery, DeliveryLimits, ProviderAccepts};

#[expect(clippy::expect_used, reason = "conformance fixture setup must succeed")]
pub async fn attachment_delivery_names_its_content(backend: Arc<dyn AttachmentStore>) {
    let bytes = b"immutable attachment original".to_vec();
    let meta = AttachmentCreateMeta::new(
        MediaType::parse("image/png").expect("MIME"),
        None,
        Some("original".into()),
    );
    let reference = backend
        .put(bytes.clone(), meta)
        .await
        .expect("put original");
    let limits = DeliveryLimits {
        max_bytes: 1024,
        valid_through_ms: 1000,
    };
    let accepts = ProviderAccepts {
        bytes: true,
        ..ProviderAccepts::NONE
    };
    let delivered = backend
        .deliver(&reference, &accepts, &limits)
        .await
        .expect("deliver original");
    assert!(matches!(delivered, Delivery::Bytes(actual) if actual == bytes));
    assert_eq!(reference.id, lash_core::attachments::content_id(&bytes));
    let runtime = RuntimeAttachmentStore::ephemeral(backend.clone());
    assert_eq!(
        runtime.read(&reference).await.expect("explicit read"),
        bytes
    );
    let mut forged = reference.clone();
    forged.byte_len += 1;
    assert!(matches!(
        runtime.read(&forged).await,
        Err(AttachmentStoreError::ContentMismatch { .. })
    ));
    assert!(matches!(
        backend.deliver(&forged, &accepts, &limits).await,
        Err(AttachmentStoreError::ContentMismatch { .. })
    ));
    assert!(matches!(
        backend
            .deliver(&reference, &ProviderAccepts::NONE, &limits)
            .await,
        Err(AttachmentStoreError::DeliveryUnsupported { .. })
    ));
    let mut absent = reference.clone();
    absent.id = lash_core::attachments::content_id(b"not uploaded");
    assert!(matches!(
        runtime.read(&absent).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
    assert!(matches!(
        backend.deliver(&absent, &accepts, &limits).await,
        Err(AttachmentStoreError::NotFound(_))
    ));
    let mut relabelled = reference.clone();
    relabelled.label = Some("same content".into());
    assert_ne!(reference, relabelled);
    assert_eq!(reference.id, relabelled.id);
    assert_eq!(
        runtime
            .read(&relabelled)
            .await
            .expect("label does not alter content"),
        bytes
    );
}
