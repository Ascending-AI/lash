mod tests {
    //! A delivery's acquisition keeps its two permanent failures apart: a
    //! store incompatibility is a typed, terminal refusal, and an attachment
    //! whose source is gone is the typed source-gone value, which acquires
    //! nothing (the contrast law of the Restate double's start-input
    //! acquisition suite).

    use std::sync::Arc;

    use lash_core_store::artifact_referrer::{ArtifactReferrer, ReferrerClaim};
    use lash_core_store::store::{
        AttachmentReferrers, AttachmentWrite, AttachmentWriteFence, AttachmentWritePermit,
        SessionReferrerState, StoreError,
    };

    use crate::runtime::attachment_delivery::{
        DeliveryAcquisition, acquire_under, deliver_output, receiving_claim, source_gone_output,
    };
    use crate::{AttachmentId, StoreSet as _};

    /// The store set's own referrers, except that every acquisition is
    /// refused as a store whose stamp this build does not read.
    struct IncompatibleAcquisitions {
        inner: Arc<dyn AttachmentReferrers>,
    }

    #[async_trait::async_trait]
    impl AttachmentReferrers for IncompatibleAcquisitions {
        async fn begin_attachment_write(
            &self,
            write: &AttachmentWrite,
        ) -> Result<AttachmentWriteFence, StoreError> {
            self.inner.begin_attachment_write(write).await
        }

        async fn complete_attachment_write(
            &self,
            write: &AttachmentWrite,
            permit: AttachmentWritePermit,
        ) -> Result<(), StoreError> {
            self.inner.complete_attachment_write(write, permit).await
        }

        async fn abort_attachment_write(
            &self,
            write: &AttachmentWrite,
            permit: AttachmentWritePermit,
        ) -> Result<(), StoreError> {
            self.inner.abort_attachment_write(write, permit).await
        }

        async fn acquire_attachment_refs(
            &self,
            _claim: &ReferrerClaim,
            _ids: &[AttachmentId],
        ) -> Result<(), StoreError> {
            Err(StoreError::Incompatible {
                refusal: crate::compat::CompatRefusal::Unstamped {
                    component: "attachment-law".into(),
                    writing_release: None,
                },
            })
        }

        async fn forget_attachment_ref(
            &self,
            referrer: &ArtifactReferrer,
            id: &AttachmentId,
        ) -> Result<(), StoreError> {
            self.inner.forget_attachment_ref(referrer, id).await
        }

        async fn end_attachment_referrer(
            &self,
            referrer: &ArtifactReferrer,
        ) -> Result<(), StoreError> {
            self.inner.end_attachment_referrer(referrer).await
        }

        async fn session_referrer_state(
            &self,
            id: &crate::SessionId,
        ) -> Result<SessionReferrerState, StoreError> {
            self.inner.session_referrer_state(id).await
        }

        async fn attachment_referrers(
            &self,
            id: &AttachmentId,
        ) -> Result<Vec<ArtifactReferrer>, StoreError> {
            self.inner.attachment_referrers(id).await
        }
    }

    #[tokio::test]
    async fn acquisition_keeps_permanent_incompatibility_and_source_gone_distinct() {
        let stores = crate::support::sqlite_memory_store_set().await;
        let referrers = stores.attachment_referrers();
        let receiver = crate::ExecutionScope::runtime_operation("contrast");
        let claim = receiving_claim(&receiver).expect("the receiver's claim");
        let delivered = lash_core_store::attachments::content_id(b"delivery bytes");

        let incompatible = IncompatibleAcquisitions {
            inner: Arc::clone(&referrers),
        };
        let acquisition = acquire_under(&incompatible, &claim, std::slice::from_ref(&delivered))
            .await
            .expect("a compatibility refusal is a typed acquisition result");
        let DeliveryAcquisition::Refused { refusal } = acquisition else {
            panic!("an incompatible store refuses the acquisition: {acquisition:?}");
        };
        assert_eq!(refusal.code, crate::RuntimeErrorCode::StoreIncompatible);
        assert!(refusal.is_terminal(), "{refusal:?}");
        assert!(
            !refusal.clone().into_runtime_error().is_retryable(),
            "{refusal:?}"
        );

        let unknown = lash_core_store::attachments::content_id(b"never uploaded");
        let output = crate::ProcessAwaitOutput::from_tool_output(
            crate::ToolCallOutput::success_tool_value(crate::ToolValue::Attachment(
                crate::AttachmentSource::stored(crate::AttachmentRef::new(
                    unknown.clone(),
                    crate::MediaType::parse("text/plain").unwrap(),
                    14,
                    None,
                    None,
                )),
            )),
        );
        let recorded = deliver_output(referrers.as_ref(), &receiver, output)
            .await
            .expect("a gone source is a typed delivery");
        assert_eq!(recorded, source_gone_output(&unknown));
        assert!(
            referrers
                .attachment_referrers(&unknown)
                .await
                .expect("read the unknown attachment's referrers")
                .is_empty(),
            "a gone source acquires nothing"
        );
    }
}
