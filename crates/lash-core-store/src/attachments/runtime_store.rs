//! Attachment access and write fencing for runtime holders.

use super::{
    AttachmentStore, AttachmentStoreError, AttachmentStorePersistence, StoredAttachment, content_id,
};
#[cfg(any(test, feature = "testing"))]
use super::{AttachmentStoreFailureClass, StoredBlobRef};
use crate::SessionId;
use crate::store::{
    AttachmentReferrers, AttachmentWrite, AttachmentWriteFence, AttachmentWritePermit, StoreError,
};
use lash_sansio::sync::MutexExt;
use lash_sansio::{AttachmentCreateMeta, AttachmentId, AttachmentRef};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

#[cfg(any(test, feature = "testing"))]
#[derive(Debug, Default)]
pub struct UnavailableAttachmentStore;

#[cfg(any(test, feature = "testing"))]
#[async_trait::async_trait]
impl AttachmentStore for UnavailableAttachmentStore {
    async fn put(
        &self,
        _bytes: Vec<u8>,
        _meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        Err(AttachmentStoreError::Backend {
            operation: "put",
            class: AttachmentStoreFailureClass::Terminal,
            source: "this context has no attachment port".into(),
        })
    }

    async fn get(
        &self,
        id: &AttachmentId,
        _max_bytes: u64,
    ) -> Result<StoredAttachment, AttachmentStoreError> {
        Err(AttachmentStoreError::NotFound(id.clone()))
    }

    async fn delete(&self, _id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        Ok(())
    }

    async fn list(&self) -> Result<Vec<StoredBlobRef>, AttachmentStoreError> {
        Ok(Vec::new())
    }

    async fn head(
        &self,
        _id: &AttachmentId,
    ) -> Result<Option<StoredBlobRef>, AttachmentStoreError> {
        Ok(None)
    }
}

/// Retry pacing when attachment reclamation holds the write fence.
/// Sleeping authorizes no deletion; a store CAS remains the authority.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentReclamationRetryPolicy {
    attempts: std::num::NonZeroU32,
    yields: u32,
    initial: std::time::Duration,
    maximum: std::time::Duration,
}
impl AttachmentReclamationRetryPolicy {
    /// Standard preset: 64 attempts, the first 8 yielding, then backoff from
    /// 2ms to 250ms. Backoff addresses remote deletes that outlast scheduler
    /// yields; these exact values have no supporting workload measurement.
    pub const fn standard() -> Self {
        Self {
            attempts: std::num::NonZeroU32::MIN.saturating_add(63),
            yields: 8,
            initial: std::time::Duration::from_millis(2),
            maximum: std::time::Duration::from_millis(250),
        }
    }
    /// Reject a delay that would spin or an inverted backoff range.
    pub fn new(
        attempts: std::num::NonZeroU32,
        yields: u32,
        initial: std::time::Duration,
        maximum: std::time::Duration,
    ) -> Result<Self, AttachmentReclamationRetryPolicyError> {
        if initial.is_zero() || initial > maximum || yields >= attempts.get() {
            return Err(AttachmentReclamationRetryPolicyError);
        }
        Ok(Self {
            attempts,
            yields,
            initial,
            maximum,
        })
    }
    pub const fn attempts(self) -> std::num::NonZeroU32 {
        self.attempts
    }
    /// Delay before the given re-acquire; zero means yield.
    pub fn delay(self, attempt: u32) -> std::time::Duration {
        if attempt <= self.yields {
            return std::time::Duration::ZERO;
        }
        let mut delay = self.initial;
        for _ in 0..(attempt - self.yields - 1).min(128) {
            delay = delay.saturating_mul(2).min(self.maximum);
            if delay == self.maximum {
                break;
            }
        }
        delay
    }
}
impl Default for AttachmentReclamationRetryPolicy {
    fn default() -> Self {
        Self::standard()
    }
}
/// Invalid attachment reclamation retry pacing.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("attachment retry requires positive ordered delays and fewer initial yields than attempts")]
pub struct AttachmentReclamationRetryPolicyError;

/// The default lifetime of an unbound session upload's staging referrer.
pub const DEFAULT_ATTACHMENT_UPLOAD_EXPIRY_MS: u64 = 86_400_000;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AttachmentHolder {
    Ephemeral,
    Runtime(crate::runtime_owner::RuntimeOwner),
}
/// Read limits, independent of put admission and retained-history policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachmentReadPolicy {
    pub max_blob_bytes: u64,
    /// Unique retained buffer capacity plus provider encoding allowance per occurrence.
    /// This bounds attachment payload work, not the allocator or the full prompt.
    pub max_request_bytes: u64,
}
impl AttachmentReadPolicy {
    pub const DEFAULT: Self = Self {
        max_blob_bytes: 32 * 1024 * 1024,
        max_request_bytes: 128 * 1024 * 1024,
    };
}

/// Attachment bytes held by a session execution, a process record, or one
/// session upload. Pending writes precede byte publication; completion records
/// upload evidence. Reads resolve content addresses; deletion releases only
/// the holder's lasting referrer edge. Reclamation alone deletes bytes.
pub struct RuntimeAttachmentStore {
    backend: Arc<dyn AttachmentStore>,
    referrers: Arc<dyn AttachmentReferrers>,
    holder: AttachmentHolder,
    max_attachment_bytes: Option<u64>,
    read_policy: AttachmentReadPolicy,
    upload_expiry_ms: u64,
    output_retention: lash_sansio::OutputRetentionPolicy,
    reclamation_retry: AttachmentReclamationRetryPolicy,
    execution: Mutex<Option<BoundAttachmentExecution>>,
    clock: Arc<dyn crate::Clock>,
}
#[derive(Clone)]
struct BoundAttachmentExecution {
    journal: lash_sansio::EffectJournalIdentity,
    recorded_puts: Arc<Mutex<BTreeSet<AttachmentId>>>,
}
pub struct AttachmentExecutionBinding {
    store: Arc<RuntimeAttachmentStore>,
    execution: BoundAttachmentExecution,
    previous: Option<BoundAttachmentExecution>,
}
impl Drop for AttachmentExecutionBinding {
    fn drop(&mut self) {
        let mut current = self.store.execution.lock_recover();
        if current
            .as_ref()
            .is_some_and(|bound| Arc::ptr_eq(&bound.recorded_puts, &self.execution.recorded_puts))
        {
            *current = self.previous.take();
        }
    }
}
impl RuntimeAttachmentStore {
    pub fn new(
        backend: Arc<dyn AttachmentStore>,
        referrers: Arc<dyn AttachmentReferrers>,
        owner: crate::runtime_owner::RuntimeOwner,
    ) -> Self {
        Self::new_with_clock(backend, referrers, owner, Arc::new(crate::SystemClock))
    }
    pub fn new_with_clock(
        backend: Arc<dyn AttachmentStore>,
        referrers: Arc<dyn AttachmentReferrers>,
        owner: crate::runtime_owner::RuntimeOwner,
        clock: Arc<dyn crate::Clock>,
    ) -> Self {
        Self {
            backend,
            referrers,
            holder: AttachmentHolder::Runtime(owner),
            max_attachment_bytes: None,
            read_policy: AttachmentReadPolicy::DEFAULT,
            upload_expiry_ms: DEFAULT_ATTACHMENT_UPLOAD_EXPIRY_MS,
            output_retention: lash_sansio::OutputRetentionPolicy::DEFAULT,
            reclamation_retry: AttachmentReclamationRetryPolicy::standard(),
            execution: Mutex::new(None),
            clock,
        }
    }
    pub fn ephemeral(backend: Arc<dyn AttachmentStore>) -> Self {
        Self {
            backend,
            referrers: Arc::new(NoopAttachmentReferrers),
            holder: AttachmentHolder::Ephemeral,
            max_attachment_bytes: None,
            read_policy: AttachmentReadPolicy::DEFAULT,
            upload_expiry_ms: DEFAULT_ATTACHMENT_UPLOAD_EXPIRY_MS,
            output_retention: lash_sansio::OutputRetentionPolicy::DEFAULT,
            reclamation_retry: AttachmentReclamationRetryPolicy::standard(),
            execution: Mutex::new(None),
            clock: Arc::new(crate::SystemClock),
        }
    }
    #[cfg(any(test, feature = "testing"))]
    pub fn unavailable() -> Self {
        Self::ephemeral(Arc::new(UnavailableAttachmentStore))
    }
    pub fn backend(&self) -> &Arc<dyn AttachmentStore> {
        &self.backend
    }
    pub fn referrers(&self) -> &Arc<dyn AttachmentReferrers> {
        &self.referrers
    }
    pub fn holder(&self) -> &AttachmentHolder {
        &self.holder
    }
    /// Select the write-fence retry preset for attachment puts.
    pub fn with_reclamation_retry(mut self, policy: AttachmentReclamationRetryPolicy) -> Self {
        self.reclamation_retry = policy;
        self
    }
    /// Reconfigure retries while preserving the bound retention and execution.
    pub fn reconfigured_reclamation_retry(&self, policy: AttachmentReclamationRetryPolicy) -> Self {
        self.reconfigured().with_reclamation_retry(policy)
    }
    pub fn reclamation_retry(&self) -> AttachmentReclamationRetryPolicy {
        self.reclamation_retry
    }
    pub fn with_max_attachment_bytes(mut self, max_attachment_bytes: Option<u64>) -> Self {
        self.max_attachment_bytes = max_attachment_bytes;
        self
    }
    pub fn with_read_policy(mut self, policy: AttachmentReadPolicy) -> Self {
        self.read_policy = policy;
        self
    }
    pub fn read_policy(&self) -> AttachmentReadPolicy {
        self.read_policy
    }
    pub fn reconfigured_read_policy(&self, policy: AttachmentReadPolicy) -> Self {
        self.reconfigured().with_read_policy(policy)
    }
    pub fn max_attachment_bytes(&self) -> Option<u64> {
        self.max_attachment_bytes
    }
    pub fn with_upload_expiry_ms(mut self, upload_expiry_ms: u64) -> Self {
        self.upload_expiry_ms = upload_expiry_ms;
        self
    }
    pub fn upload_expiry_ms(&self) -> u64 {
        self.upload_expiry_ms
    }
    pub fn reconfigured_max_attachment_bytes(&self, max_attachment_bytes: Option<u64>) -> Self {
        Self {
            max_attachment_bytes,
            ..self.reconfigured()
        }
    }

    /// The byte policy an output is measured against before it enters
    /// history (FIG-1643). Process configuration: every step that applies it
    /// journals the policy it applied, so a replay never reads it again.
    pub fn output_retention(&self) -> lash_sansio::OutputRetentionPolicy {
        self.output_retention
    }

    /// This store with `output_retention` as its retention policy.
    pub fn with_output_retention(
        mut self,
        output_retention: lash_sansio::OutputRetentionPolicy,
    ) -> Self {
        self.output_retention = output_retention;
        self
    }

    /// A copy of this store under `output_retention`, keeping its bound
    /// execution, as [`Self::reconfigured_max_attachment_bytes`] does for the
    /// size limit.
    pub fn reconfigured_output_retention(
        &self,
        output_retention: lash_sansio::OutputRetentionPolicy,
    ) -> Self {
        Self {
            output_retention,
            ..self.reconfigured()
        }
    }

    fn reconfigured(&self) -> Self {
        Self {
            backend: Arc::clone(&self.backend),
            referrers: Arc::clone(&self.referrers),
            holder: self.holder.clone(),
            max_attachment_bytes: self.max_attachment_bytes,
            read_policy: self.read_policy,
            upload_expiry_ms: self.upload_expiry_ms,
            output_retention: self.output_retention,
            reclamation_retry: self.reclamation_retry,
            execution: Mutex::new(self.execution.lock_recover().clone()),
            clock: Arc::clone(&self.clock),
        }
    }
    pub fn persistence(&self) -> AttachmentStorePersistence {
        self.backend.persistence()
    }
    pub fn bind_execution_scoped(
        self: &Arc<Self>,
        journal: lash_sansio::EffectJournalIdentity,
    ) -> Result<AttachmentExecutionBinding, AttachmentStoreError> {
        if !matches!(
            self.holder,
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Session(_))
        ) {
            return Err(AttachmentStoreError::Contract(
                "only a session runtime can bind an attachment execution".into(),
            ));
        }
        let execution = BoundAttachmentExecution {
            journal,
            recorded_puts: Arc::new(Mutex::new(BTreeSet::new())),
        };
        let previous = self.execution.lock_recover().replace(execution.clone());
        Ok(AttachmentExecutionBinding {
            store: Arc::clone(self),
            execution,
            previous,
        })
    }
    pub fn recorded_execution_puts(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> BTreeSet<AttachmentId> {
        let recorded = self
            .execution
            .lock_recover()
            .as_ref()
            .filter(|bound| &bound.journal == journal)
            .map(|bound| Arc::clone(&bound.recorded_puts));
        recorded
            .map(|puts| puts.lock_recover().clone())
            .unwrap_or_default()
    }
    pub async fn put(
        &self,
        bytes: Vec<u8>,
        meta: AttachmentCreateMeta,
    ) -> Result<AttachmentRef, AttachmentStoreError> {
        let byte_len = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        if let Some(max_bytes) = self.max_attachment_bytes
            && byte_len > max_bytes
        {
            return Err(AttachmentStoreError::SizeLimitExceeded {
                byte_len,
                max_bytes,
            });
        }
        let attachment_id = content_id(&bytes);
        let execution = self.execution.lock_recover().clone();
        let claim = match &self.holder {
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Process(id)) => {
                crate::artifact_referrer::ReferrerClaim::unguarded(
                    crate::artifact_referrer::ArtifactReferrer::ProcessRecord(id.clone()),
                )
            }
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Session(id)) => {
                match &execution {
                    Some(bound) => Ok(crate::artifact_referrer::ReferrerClaim::guarded(
                        crate::artifact_referrer::ReferrerGuard::Journal(bound.journal.clone()),
                    )),
                    None => Ok(crate::artifact_referrer::ReferrerClaim::guarded(
                        crate::artifact_referrer::ReferrerGuard::Upload {
                            upload: crate::artifact_referrer::UploadReferrerId::mint(id.clone()),
                            expires_at_ms: self
                                .clock
                                .timestamp_ms()
                                .saturating_add(self.upload_expiry_ms),
                        },
                    )),
                }
            }
            AttachmentHolder::Ephemeral => crate::artifact_referrer::ReferrerClaim::unguarded(
                crate::artifact_referrer::ArtifactReferrer::Session(
                    SessionId::parse("ephemeral").expect("the ephemeral session id is nonblank"),
                ),
            ),
        }
        .map_err(|error| AttachmentStoreError::Contract(error.to_string()))?;
        let write = AttachmentWrite {
            attachment_id: attachment_id.clone(),
            claim,
        };
        // Acquire the write fence first: the intent is recorded before any bytes
        // land (the write-ahead guarantee) and, in the same mutation, the digest
        // is taken back from a sweep that condemned it. If this fails the bytes
        // never land.
        let mut attempts: u32 = 0;
        let reference = loop {
            attempts += 1;
            let fence = self
                .referrers
                .begin_attachment_write(&write)
                .await
                .map_err(|source| AttachmentStoreError::ReferrersOperationFailed {
                    operation: "begin_attachment_write",
                    attachment_id: attachment_id.clone(),
                    source: Box::new(source),
                })?;
            let permit = match fence {
                AttachmentWriteFence::Granted(permit) => permit,
                // A sweep armed this digest's delete before we recorded an
                // intent. Writing bytes into an in-flight delete would lose
                // them, so back off and re-acquire: the sweep releases the
                // digest as soon as the delete lands, and the granted retry
                // re-puts the content. The delay paces the retry; it authorizes
                // nothing and expires nothing.
                AttachmentWriteFence::ReclamationInFlight => {
                    if attempts >= self.reclamation_retry.attempts().get() {
                        return Err(AttachmentStoreError::ReclamationInFlight {
                            attachment_id,
                            attempts,
                        });
                    }
                    self.clock
                        .sleep(self.reclamation_retry.delay(attempts))
                        .await;
                    continue;
                }
            };
            // Cloned per attempt because a retry has to re-put: a sweep that
            // condemned the digest in the window below deletes both this row and
            // the bytes, so the recovering attempt is a full re-put, not a bare
            // re-stamp.
            let reference = match self.backend.put(bytes.clone(), meta.clone()).await {
                Ok(reference) => reference,
                Err(backend_error) => {
                    if let Err(rollback_error) =
                        self.referrers.abort_attachment_write(&write, permit).await
                    {
                        return Err(AttachmentStoreError::WriteRollbackFailed {
                            attachment_id,
                            write_error: Box::new(backend_error),
                            abort_error: Box::new(rollback_error),
                        });
                    }
                    return Err(backend_error);
                }
            };
            if reference.id != attachment_id
                || reference.byte_len != bytes.len() as u64
                || reference.media_type != meta.media_type
                || reference.type_metadata != meta.type_metadata
                || reference.label != meta.label
            {
                let backend_error = AttachmentStoreError::Contract(format!(
                    "attachment store returned a different content reference after a pending write for `{attachment_id}` (returned id `{}`)",
                    reference.id
                ));
                if let Err(rollback_error) =
                    self.referrers.abort_attachment_write(&write, permit).await
                {
                    return Err(AttachmentStoreError::WriteRollbackFailed {
                        attachment_id,
                        write_error: Box::new(backend_error),
                        abort_error: Box::new(rollback_error),
                    });
                }
                return Err(backend_error);
            }
            match self
                .referrers
                .complete_attachment_write(&write, permit)
                .await
            {
                Ok(()) => break reference,
                // A permit explicitly retired by recovery or referrer ending
                // cannot stamp evidence. Retry behind a fresh fence: a permanent
                // end refuses the next begin, while a recovered write may resume.
                Err(StoreError::StaleWritePermit { .. })
                    if attempts < self.reclamation_retry.attempts().get() =>
                {
                    self.clock
                        .sleep(self.reclamation_retry.delay(attempts))
                        .await;
                    continue;
                }
                Err(source) => {
                    return Err(AttachmentStoreError::ReferrersOperationFailed {
                        operation: "complete_attachment_write",
                        attachment_id,
                        source: Box::new(source),
                    });
                }
            }
        };
        if let Some(bound) = &execution {
            bound.recorded_puts.lock_recover().insert(attachment_id);
        }
        Ok(reference)
    }

    pub async fn read(&self, reference: &AttachmentRef) -> Result<Vec<u8>, AttachmentStoreError> {
        let stored = self
            .backend
            .get(&reference.id, self.read_policy.max_blob_bytes)
            .await?;
        super::validate_attachment_bytes(
            reference,
            &stored.bytes,
            stored.bytes.capacity() as u64,
            self.read_policy.max_blob_bytes,
        )?;
        Ok(stored.bytes)
    }

    /// Host upload access keeps the session holder and clears any live tool execution.
    pub fn unbound(&self) -> Self {
        Self {
            execution: Mutex::new(None),
            ..self.reconfigured()
        }
    }

    /// Replace the infrastructure port, preserving the holder and its limits.
    pub fn reconfigured_backend(&self, backend: Arc<dyn AttachmentStore>) -> Self {
        Self {
            backend,
            ..self.reconfigured()
        }
    }
    /// The claims whose stored attachment this holder's process record
    /// holds. A process terminal must show this provenance before a value it
    /// built delivers an attachment typed (ADR 0124 §4): the record holds
    /// the process's own puts, its deliveries and its start inputs. A claim
    /// the record does not hold is dropped. No other holder vouches for any
    /// claim.
    pub async fn claims_held_by_process_record(
        &self,
        claims: Vec<AttachmentRef>,
    ) -> Result<Vec<AttachmentRef>, AttachmentStoreError> {
        let AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Process(process_id)) =
            &self.holder
        else {
            return Ok(Vec::new());
        };
        let record = crate::artifact_referrer::ArtifactReferrer::ProcessRecord(process_id.clone());
        let mut held = Vec::new();
        for claim in claims {
            let attachment_ref = &claim;
            let referrers = self
                .referrers
                .attachment_referrers(&attachment_ref.id)
                .await
                .map_err(|source| AttachmentStoreError::ReferrersOperationFailed {
                    operation: "attachment_referrers",
                    attachment_id: attachment_ref.id.clone(),
                    source: Box::new(source),
                })?;
            if referrers.contains(&record) && !held.contains(&claim) {
                held.push(claim);
            }
        }
        Ok(held)
    }
    pub async fn delete(&self, id: &AttachmentId) -> Result<(), AttachmentStoreError> {
        let referrer = match &self.holder {
            AttachmentHolder::Ephemeral => return Ok(()),
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Session(id)) => {
                crate::artifact_referrer::ArtifactReferrer::Session(id.clone())
            }
            AttachmentHolder::Runtime(crate::runtime_owner::RuntimeOwner::Process(id)) => {
                crate::artifact_referrer::ArtifactReferrer::ProcessRecord(id.clone())
            }
        };
        self.referrers
            .forget_attachment_ref(&referrer, id)
            .await
            .map_err(|source| AttachmentStoreError::ReferrersOperationFailed {
                operation: "forget_attachment_ref",
                attachment_id: id.clone(),
                source: Box::new(source),
            })
    }
}
pub struct NoopAttachmentReferrers;
crate::impl_noop_attachment_referrers!(NoopAttachmentReferrers);
/// The attachment port of a runtime store.
pub struct PersistenceReferrersAdapter(pub Arc<dyn crate::RuntimeStore>);
#[async_trait::async_trait]
impl AttachmentReferrers for PersistenceReferrersAdapter {
    async fn begin_attachment_write(
        &self,
        write: &AttachmentWrite,
    ) -> Result<AttachmentWriteFence, StoreError> {
        self.0.begin_attachment_write(write).await
    }
    async fn complete_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.0.complete_attachment_write(write, permit).await
    }
    async fn abort_attachment_write(
        &self,
        write: &AttachmentWrite,
        permit: AttachmentWritePermit,
    ) -> Result<(), StoreError> {
        self.0.abort_attachment_write(write, permit).await
    }
    async fn acquire_attachment_refs(
        &self,
        claim: &crate::artifact_referrer::ReferrerClaim,
        ids: &[AttachmentId],
    ) -> Result<(), StoreError> {
        self.0.acquire_attachment_refs(claim, ids).await
    }
    async fn forget_attachment_ref(
        &self,
        referrer: &crate::artifact_referrer::ArtifactReferrer,
        id: &AttachmentId,
    ) -> Result<(), StoreError> {
        self.0.forget_attachment_ref(referrer, id).await
    }
    async fn end_attachment_referrer(
        &self,
        referrer: &crate::artifact_referrer::ArtifactReferrer,
    ) -> Result<(), StoreError> {
        self.0.end_attachment_referrer(referrer).await
    }
    async fn session_referrer_state(
        &self,
        session: &SessionId,
    ) -> Result<crate::store::SessionReferrerState, StoreError> {
        self.0.session_referrer_state(session).await
    }
    async fn attachment_referrers(
        &self,
        id: &AttachmentId,
    ) -> Result<Vec<crate::artifact_referrer::ArtifactReferrer>, StoreError> {
        self.0.attachment_referrers(id).await
    }
}
