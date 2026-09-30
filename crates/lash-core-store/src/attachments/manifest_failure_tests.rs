use super::*;
use crate::artifact_referrer::{ArtifactReferrer, ReferrerClaim};
use crate::runtime_owner::RuntimeOwner;
use std::error::Error;
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Copy, Debug)]
enum Cause {
    Storage,
    Contended,
    Backend,
    WriterFenced,
    Incompatible,
}

const CAUSES: [Cause; 5] = [
    Cause::Storage,
    Cause::Contended,
    Cause::Backend,
    Cause::WriterFenced,
    Cause::Incompatible,
];

impl Cause {
    fn error(self) -> StoreError {
        match self {
            Self::Storage => StoreError::StorageFailure {
                backend: "manifest-probe",
                message: "write unavailable".to_string(),
            },
            Self::Contended => StoreError::Contended,
            Self::Backend => StoreError::Backend("manifest transport failed".to_string()),
            Self::WriterFenced => StoreError::WriterFenced {
                recorded: 3,
                writable: crate::compat::VersionRange::between(1, 2),
            },
            Self::Incompatible => StoreError::Incompatible {
                refusal: crate::compat::CompatRefusal::UnknownVocabulary {
                    surface: "attachment manifest".to_string(),
                    label: "future owner".to_string(),
                },
            },
        }
    }

    fn assert_preserved(self, error: &StoreError) {
        match (self, error) {
            (Self::Storage, StoreError::StorageFailure { backend, message }) => {
                assert_eq!(*backend, "manifest-probe");
                assert_eq!(message, "write unavailable");
            }
            (Self::Contended, StoreError::Contended) => {}
            (Self::Backend, StoreError::Backend(message)) => {
                assert_eq!(message, "manifest transport failed");
            }
            (Self::WriterFenced, StoreError::WriterFenced { recorded, writable }) => {
                assert_eq!(*recorded, 3);
                assert_eq!(*writable, crate::compat::VersionRange::between(1, 2));
            }
            (Self::Incompatible, StoreError::Incompatible { refusal }) => {
                assert_eq!(
                    *refusal,
                    crate::compat::CompatRefusal::UnknownVocabulary {
                        surface: "attachment manifest".to_string(),
                        label: "future owner".to_string(),
                    }
                );
            }
            _ => panic!("lost {self:?}: {error:?}"),
        }
    }
}

#[derive(Clone, Copy)]
enum Operation {
    Begin,
    Complete,
    Forget,
    Abort,
}

struct FailingManifest {
    operation: Operation,
    cause: Cause,
    calls: Mutex<Vec<&'static str>>,
}

impl FailingManifest {
    fn record(&self, operation: &'static str) {
        self.calls.lock_recover().push(operation);
    }
}

#[async_trait::async_trait]
impl AttachmentManifest for FailingManifest {
    async fn begin_attachment_write(
        &self,
        intent: &AttachmentWrite,
    ) -> Result<AttachmentWriteFence, StoreError> {
        self.record("begin");
        if matches!(self.operation, Operation::Begin) {
            return Err(self.cause.error());
        }
        NoopAttachmentManifest.begin_attachment_write(intent).await
    }

    async fn complete_attachment_write(
        &self,
        _intent: &AttachmentWrite,
        _permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.record("complete");
        assert!(matches!(self.operation, Operation::Complete));
        Err(self.cause.error())
    }

    async fn abort_attachment_write(
        &self,
        _intent: &AttachmentWrite,
        _permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.record("abort");
        assert!(matches!(self.operation, Operation::Abort));
        Err(self.cause.error())
    }

    async fn forget_attachment_ref(
        &self,
        _referrer: &ArtifactReferrer,
        _id: &AttachmentId,
    ) -> Result<(), StoreError> {
        self.record("forget");
        assert!(matches!(self.operation, Operation::Forget));
        Err(self.cause.error())
    }

    async fn acquire_attachment_refs(
        &self,
        _claim: &ReferrerClaim,
        _ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        panic!("unexpected acquire")
    }
    async fn end_attachment_referrer(&self, _r: &ArtifactReferrer) -> Result<(), StoreError> {
        panic!("unexpected end")
    }
    async fn session_referrer_state(
        &self,
        _s: &SessionId,
    ) -> Result<crate::SessionReferrerState, StoreError> {
        panic!("unexpected state")
    }
    async fn attachment_referrers(
        &self,
        _id: &AttachmentId,
    ) -> Result<Vec<ArtifactReferrer>, StoreError> {
        panic!("unexpected referrers")
    }
}

#[derive(Clone, Copy)]
enum PutResult {
    Success,
    WrongId,
    Failure(AttachmentStoreFailureClass),
}

struct ProbeBackend {
    result: PutResult,
    puts: AtomicUsize,
}

#[async_trait::async_trait]
impl AttachmentStore for ProbeBackend {
    async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        self.puts.fetch_add(1, Ordering::SeqCst);
        if let PutResult::Failure(class) = self.result {
            return Err(AttachmentStoreError::Backend {
                operation: "put",
                class,
                source: Box::new(std::io::Error::other("blob transport failed")),
            });
        }
        let id = match self.result {
            PutResult::WrongId => AttachmentId::parse("wrong-id").unwrap(),
            _ => content_id(&bytes),
        };
        Ok(AttachmentRef::new(
            id,
            meta.media_type,
            bytes.len() as u64,
            meta.type_metadata,
            meta.label,
        ))
    }

    async fn get(&self, _id: &AttachmentId) -> Result<StoredAttachment, AttachmentStoreError> {
        panic!("unexpected get")
    }

    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        panic!("session deletion must not delete physical bytes")
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        panic!("unexpected list")
    }

    async fn head(
        &self,
        _id: &AttachmentId,
    ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        panic!("unexpected head")
    }
}

fn probe(
    operation: Operation,
    cause: Cause,
    result: PutResult,
) -> (
    SessionAttachmentStore,
    Arc<FailingManifest>,
    Arc<ProbeBackend>,
) {
    let manifest = Arc::new(FailingManifest {
        operation,
        cause,
        calls: Mutex::new(Vec::new()),
    });
    let backend = Arc::new(ProbeBackend {
        result,
        puts: AtomicUsize::new(0),
    });
    (
        SessionAttachmentStore::new(
            backend.clone(),
            manifest.clone(),
            RuntimeOwner::Session("manifest-failure-session".into()),
        ),
        manifest,
        backend,
    )
}

async fn put(store: &SessionAttachmentStore) -> AttachmentStoreError {
    store
        .put(
            b"manifest failure probe".to_vec(),
            AttachmentCreateMeta::new(
                lash_sansio::MediaType::parse("text/plain").unwrap(),
                None,
                None,
            ),
        )
        .await
        .expect_err("injected failure")
}

async fn assert_manifest_failure(operation: Operation) {
    for cause in CAUSES {
        let (store, manifest, backend) = probe(operation, cause, PutResult::Success);
        let error = if matches!(operation, Operation::Forget) {
            store
                .delete(&content_id(b"manifest failure probe"))
                .await
                .expect_err("injected forget failure")
        } else {
            put(&store).await
        };
        let (calls, puts) = match operation {
            Operation::Begin => (vec!["begin"], 0),
            Operation::Complete => (vec!["begin", "complete"], 1),
            Operation::Forget => (vec!["forget"], 0),
            Operation::Abort => unreachable!(),
        };
        assert_eq!(*manifest.calls.lock_recover(), calls);
        assert_eq!(
            backend.puts.load(Ordering::SeqCst),
            puts,
            "no publication retry"
        );
        assert_eq!(
            error.is_retryable(),
            cause.error().is_transient(),
            "{cause:?}: {error:?}"
        );
        let source = error
            .source()
            .and_then(|source| source.downcast_ref::<Box<StoreError>>().map(Box::as_ref))
            .expect("manifest adapter must expose the typed StoreError source");
        cause.assert_preserved(source);
        let AttachmentStoreError::ManifestOperationFailed {
            operation: actual_operation,
            attachment_id,
            ..
        } = &error
        else {
            panic!("manifest operation context must be structured");
        };
        let expected_operation = match operation {
            Operation::Begin => "begin_attachment_write",
            Operation::Complete => "complete_attachment_write",
            Operation::Forget => "forget_attachment_ref",
            Operation::Abort => unreachable!(),
        };
        assert_eq!(*actual_operation, expected_operation);
        assert_eq!(*attachment_id, content_id(b"manifest failure probe"));
    }
}

#[tokio::test]
async fn begin_write_preserves_store_causes_and_classification() {
    assert_manifest_failure(Operation::Begin).await;
}

#[tokio::test]
async fn complete_write_preserves_store_causes_and_classification() {
    assert_manifest_failure(Operation::Complete).await;
}

#[tokio::test]
async fn remove_reference_preserves_store_causes_and_classification() {
    assert_manifest_failure(Operation::Forget).await;
}

#[tokio::test]
async fn backend_write_abort_failure_preserves_causes_and_conservative_classification() {
    for class in [
        AttachmentStoreFailureClass::Transient,
        AttachmentStoreFailureClass::Terminal,
        AttachmentStoreFailureClass::Credentials,
    ] {
        for cause in CAUSES {
            let (store, manifest, backend) =
                probe(Operation::Abort, cause, PutResult::Failure(class));
            let error = put(&store).await;
            assert_eq!(*manifest.calls.lock_recover(), vec!["begin", "abort"]);
            assert_eq!(backend.puts.load(Ordering::SeqCst), 1);
            assert_eq!(
                error.is_retryable(),
                class.is_retryable() && cause.error().is_transient()
            );
            let write = error
                .source()
                .and_then(|source| {
                    source
                        .downcast_ref::<Box<AttachmentStoreError>>()
                        .map(Box::as_ref)
                })
                .expect("rollback must expose the original typed write error");
            assert_eq!(write.failure_class(), Some(class));
            assert_eq!(write.source().unwrap().to_string(), "blob transport failed");
            assert_rollback_cause(&error, cause);
            assert!(
                error.to_string().contains(&cause.error().to_string()),
                "abort diagnostics must survive"
            );
        }
    }
}

#[tokio::test]
async fn wrong_id_abort_failure_preserves_terminal_contract() {
    for cause in CAUSES {
        let (store, manifest, backend) = probe(Operation::Abort, cause, PutResult::WrongId);
        let error = put(&store).await;
        assert_eq!(*manifest.calls.lock_recover(), vec!["begin", "abort"]);
        assert_eq!(backend.puts.load(Ordering::SeqCst), 1);
        assert!(!error.is_retryable(), "contract failure remains terminal");
        let write = error
            .source()
            .and_then(|source| {
                source
                    .downcast_ref::<Box<AttachmentStoreError>>()
                    .map(Box::as_ref)
            })
            .expect("rollback must expose the typed contract failure");
        assert!(
            matches!(write, AttachmentStoreError::Contract(message) if message.contains("wrong-id"))
        );
        assert_rollback_cause(&error, cause);
        assert!(
            error.to_string().contains(&cause.error().to_string()),
            "abort diagnostics must survive"
        );
    }
}

fn assert_rollback_cause(error: &AttachmentStoreError, cause: Cause) {
    let AttachmentStoreError::WriteRollbackFailed {
        attachment_id,
        abort_error,
        ..
    } = error
    else {
        panic!("rollback must retain both typed causes");
    };
    assert_eq!(*attachment_id, content_id(b"manifest failure probe"));
    cause.assert_preserved(abort_error);
}
