//! ATTACHMENT-IDENTITY and DELIVERY-BOUNDS at the signing boundary.
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Debug, Default)]
struct SigningProbe {
    seconds: AtomicU64,
}
#[async_trait::async_trait]
impl Signer for SigningProbe {
    async fn signed_url(
        &self,
        method: Method,
        path: &Path,
        expires: std::time::Duration,
    ) -> object_store::Result<Url> {
        assert_eq!(method, Method::GET);
        self.seconds.store(expires.as_secs(), Ordering::SeqCst);
        Ok(Url::parse(&format!(
            "https://attachments.invalid/{path}?delivery-secret"
        ))
        .unwrap())
    }
}
#[tokio::test]
async fn url_delivery_names_the_put_object_and_respects_the_fetch_horizon() {
    let objects = Arc::new(object_store::memory::InMemory::new());
    let signer = Arc::new(SigningProbe::default());
    let store = S3AttachmentStore::from_object_store_with_signer(
        objects.clone(),
        signer.clone(),
        Some("originals".into()),
    );
    let bytes = b"immutable signed content".to_vec();
    let meta = AttachmentCreateMeta::new(
        lash_sansio::MediaType::parse("image/png").unwrap(),
        None,
        None,
    );
    let reference = store.put(bytes.clone(), meta).await.unwrap();
    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let horizon = now + 120_000;
    let limits = DeliveryLimits {
        max_bytes: 1024,
        max_upload_bytes: 1024,
        valid_through_ms: horizon,
    };
    let accepts = ProviderAccepts {
        bytes: true,
        url: true,
        provider_file: None,
    };
    let delivery = store.deliver(&reference, &accepts, &limits).await.unwrap();
    let Delivery::Url {
        url,
        valid_until_ms,
    } = &delivery
    else {
        panic!("URL-capable route receives a signature");
    };
    assert!(valid_until_ms.unwrap() >= horizon);
    assert!((120..=121).contains(&signer.seconds.load(Ordering::SeqCst)));
    assert!(
        url.expose()
            .contains(store.content_path(&reference.id).unwrap().as_ref())
    );
    assert!(!format!("{delivery:?}").contains("delivery-secret"));
    assert_eq!(
        objects
            .get(&store.content_path(&reference.id).unwrap())
            .await
            .unwrap()
            .bytes()
            .await
            .unwrap()
            .as_ref(),
        bytes
    );
    let byte_only = ProviderAccepts {
        bytes: true,
        ..ProviderAccepts::NONE
    };
    assert!(
        matches!(store.deliver(&reference, &byte_only, &limits).await.unwrap(), Delivery::Bytes(value) if value == bytes)
    );
    let long = DeliveryLimits {
        valid_through_ms: now + 8 * 24 * 60 * 60 * 1000,
        ..limits
    };
    assert!(
        matches!(store.deliver(&reference, &accepts, &long).await.unwrap(), Delivery::Bytes(value) if value == bytes)
    );
    assert!(matches!(
        store
            .deliver(
                &reference,
                &ProviderAccepts {
                    bytes: false,
                    ..accepts
                },
                &long
            )
            .await,
        Err(AttachmentStoreError::DeliveryUnsupported { .. })
    ));
    let mut forged = reference;
    forged.byte_len += 1;
    assert!(matches!(
        store.deliver(&forged, &byte_only, &limits).await,
        Err(AttachmentStoreError::ContentMismatch { .. })
    ));
    assert!(matches!(
        store
            .deliver(
                &forged,
                &ProviderAccepts {
                    bytes: false,
                    url: true,
                    provider_file: None
                },
                &limits
            )
            .await,
        Err(AttachmentStoreError::ContentMismatch { .. })
    ));
}

// D-URLOPTIN: a configured store delivers bytes unless the host opts in.
#[test]
fn presigned_url_delivery_is_a_builder_opt_in() {
    let builder = || {
        S3AttachmentStore::builder("bucket", "us-east-1")
            .access_key_id("key")
            .secret_access_key("secret")
    };
    assert!(builder().build().unwrap().signer.is_none());
    assert!(
        builder()
            .presigned_url_delivery(true)
            .build()
            .unwrap()
            .signer
            .is_some()
    );
}
