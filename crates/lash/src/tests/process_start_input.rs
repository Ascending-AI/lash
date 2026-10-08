//! A host process start whose turn input carries a host upload (ADR 0113
//! §3.3): the start stages the upload under its own referrer before it
//! registers, so the registered process holds the bytes on its own, apart
//! from the upload's expiring edge; an input that is not there to stage is
//! refused before anything is registered.

use super::*;

const SESSION: &str = "upload-session";

/// A core over `backend` whose sessions are on a served mock profile, with
/// the host's session created.
async fn upload_core(backend: lash_core::Backend) -> Result<LashCore> {
    let core = explicit_ephemeral_facets(LashCore::standard_builder(backend))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    core.session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(mock_session_spec()))
        .await?;
    Ok(core)
}

/// The host's upload store over `core`'s backend.
fn uploads(core: &LashCore) -> lash_core::facade_support::RuntimeAttachmentStore {
    lash_core::facade_support::RuntimeAttachmentStore::new(
        core.backend().attachment_store(),
        core.backend().attachment_referrers(),
        lash_core::RuntimeOwner::Session(SESSION.into()),
    )
    .with_upload_expiry_ms(1_000)
}

/// A detached `SessionTurn` start whose child turn reads `input`.
fn request(input: lash_core::AttachmentRef) -> lash_core::ProcessStartRequest {
    lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::SessionTurn {
            definition_key: "uploaded-start-input".into(),
            create_request: Box::new(lash_core::SessionCreateRequest::child(
                SESSION,
                lash_core::SessionStartPoint::Empty,
                lash_core::SessionPolicy {
                    model: Some(recorded_llm_profile(mock_llm_profile_spec())),
                    ..lash_core::SessionPolicy::new(
                        crate::TurnBudget::Unbounded,
                        crate::MaxToolCalls::new(1024),
                        crate::NoProgressBudget::bounded(12),
                    )
                },
                lash_core::PluginOptions::default(),
            )),
            turn_input: Box::new(
                crate::TurnInput::text("read the host upload")
                    .with_attachment(lash_core::AttachmentSource::stored(input)),
            ),
            result: lash_core::SessionTurnOutcome::Turn,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
}

fn octets() -> lash_core::AttachmentCreateMeta {
    lash_core::AttachmentCreateMeta::new(
        lash_core::MediaType::parse("application/octet-stream").expect("a media type"),
        None,
        None,
    )
}

/// Every process the registry holds.
async fn processes(core: &LashCore) -> Result<Vec<lash_core::ProcessRecord>> {
    Ok(core
        .backend()
        .process_registry()
        .list_processes(&lash_core::ProcessListFilter {
            status: lash_core::ProcessStatusFilter::Any,
            ..lash_core::ProcessListFilter::default()
        })
        .await?)
}

/// A started process holds its uploaded input under its own record: the
/// record names the input, and the input's referrers include the process
/// beside the upload's expiring edge, so the upload's end cannot reclaim it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_host_uploaded_start_input_is_held_by_its_registered_process() -> Result<()> {
    let core = upload_core(sqlite_memory_store_backend().await).await?;
    let bytes = b"unbound host upload\0\xff exact bytes".to_vec();
    let input = uploads(&core)
        .put(bytes.clone(), octets())
        .await
        .expect("the upload is stored");
    let started = core
        .processes()
        .start(request(input.clone()), core.effect_host())
        .await?;
    let record = core
        .backend()
        .process_registry()
        .get_process(&started.process_id)
        .await?
        .expect("the started process is registered");
    assert_eq!(record.input.stored_attachment_ids(), vec![input.id.clone()]);
    let referrers = core
        .backend()
        .attachment_referrers()
        .attachment_referrers(&input.id)
        .await
        .expect("the input's referrers read");
    assert!(
        referrers.contains(&lash_core::ArtifactReferrer::ProcessRecord(
            started.process_id.clone()
        )),
        "the registered process holds its input on its own: {referrers:?}"
    );
    assert_eq!(
        core.backend()
            .attachment_store()
            .get(&input.id, bytes.len() as u64)
            .await
            .expect("the input's bytes are kept")
            .bytes,
        bytes
    );
    core.shutdown().await?;
    Ok(())
}

/// A start whose uploaded input is not in this deployment's store is
/// refused before registration: no process is registered.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unavailable_host_uploaded_start_input_is_refused_before_registration() -> Result<()> {
    let core = upload_core(sqlite_memory_store_backend().await).await?;
    // An upload another deployment's store holds: this one has no bytes
    // and no referrer for it.
    let elsewhere = upload_core(sqlite_memory_store_backend().await).await?;
    let input = uploads(&elsewhere)
        .put(b"uploaded elsewhere".to_vec(), octets())
        .await
        .expect("the upload is stored");
    elsewhere.shutdown().await?;
    let refused = core
        .processes()
        .start(request(input), core.effect_host())
        .await;
    assert!(
        refused.is_err(),
        "the missing upload is refused: {refused:?}"
    );
    assert!(
        processes(&core).await?.is_empty(),
        "nothing is registered for a refused start"
    );
    core.shutdown().await?;
    Ok(())
}

/// The store set's own referrers, except that the first acquisition under
/// a start's input-staging claim meets a live, contended store.
struct ContendedOnce {
    inner: Arc<dyn lash_core::AttachmentReferrers>,
    faulted: Arc<AtomicUsize>,
}

#[async_trait]
impl lash_core::AttachmentReferrers for ContendedOnce {
    async fn begin_attachment_write(
        &self,
        write: &lash_core::store::AttachmentWrite,
    ) -> std::result::Result<lash_core::store::AttachmentWriteFence, StoreError> {
        self.inner.begin_attachment_write(write).await
    }

    async fn complete_attachment_write(
        &self,
        write: &lash_core::store::AttachmentWrite,
        permit: lash_core::store::AttachmentWritePermit,
    ) -> std::result::Result<(), StoreError> {
        self.inner.complete_attachment_write(write, permit).await
    }

    async fn abort_attachment_write(
        &self,
        write: &lash_core::store::AttachmentWrite,
        permit: lash_core::store::AttachmentWritePermit,
    ) -> std::result::Result<(), StoreError> {
        self.inner.abort_attachment_write(write, permit).await
    }

    async fn acquire_attachment_refs(
        &self,
        claim: &lash_core::ReferrerClaim,
        ids: &[lash_core::AttachmentId],
    ) -> std::result::Result<(), StoreError> {
        if matches!(
            claim.referrer(),
            lash_core::ArtifactReferrer::StartInput { .. }
        ) && self.faulted.fetch_add(1, Ordering::SeqCst) == 0
        {
            return Err(StoreError::Contended);
        }
        self.inner.acquire_attachment_refs(claim, ids).await
    }

    async fn forget_attachment_ref(
        &self,
        referrer: &lash_core::ArtifactReferrer,
        id: &lash_core::AttachmentId,
    ) -> std::result::Result<(), StoreError> {
        self.inner.forget_attachment_ref(referrer, id).await
    }

    async fn end_attachment_referrer(
        &self,
        referrer: &lash_core::ArtifactReferrer,
    ) -> std::result::Result<(), StoreError> {
        self.inner.end_attachment_referrer(referrer).await
    }

    async fn session_referrer_state(
        &self,
        id: &lash_core::SessionId,
    ) -> std::result::Result<lash_core::store::SessionReferrerState, StoreError> {
        self.inner.session_referrer_state(id).await
    }

    async fn attachment_referrers(
        &self,
        id: &lash_core::AttachmentId,
    ) -> std::result::Result<Vec<lash_core::ArtifactReferrer>, StoreError> {
        self.inner.attachment_referrers(id).await
    }
}

/// A live store fault while a start acquires its uploaded input is not the
/// start's refusal: the host's retry under the same start key registers one
/// process, which holds the input.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_start_input_acquisition_fault_retries_the_same_registered_process() -> Result<()> {
    let stores: Arc<dyn lash_core::StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory stores"),
    );
    let faulted = Arc::new(AtomicUsize::new(0));
    let layered = lash_core::testing::runtime_helpers::LayeredStores::over(stores)
        .map_attachment_referrers(|inner| -> Arc<dyn lash_core::AttachmentReferrers> {
            Arc::new(ContendedOnce {
                inner,
                faulted: Arc::clone(&faulted),
            })
        })
        .into_store_set();
    let core = upload_core(lash_conformance::backend_over(layered)).await?;
    let input = uploads(&core)
        .put(b"retried upload".to_vec(), octets())
        .await
        .expect("the upload is stored");
    const KEY: &str = "uploaded-start-retry";
    let processes_api = core.processes();
    let first = processes_api
        .start(
            request(input.clone()).with_host_start_key(KEY),
            core.effect_host(),
        )
        .await;
    let started = match first {
        Ok(started) => started,
        Err(error) => {
            assert!(
                !error.to_string().contains("incompatib"),
                "a live fault is no permanent refusal: {error}"
            );
            processes_api
                .start(
                    request(input.clone()).with_host_start_key(KEY),
                    core.effect_host(),
                )
                .await?
        }
    };
    assert!(
        faulted.load(Ordering::SeqCst) >= 2,
        "the first input acquisition met the fault and a later one ran"
    );
    let registered = processes(&core).await?;
    assert_eq!(
        registered.len(),
        1,
        "one start key, one process: {registered:?}"
    );
    assert_eq!(registered[0].id, started.process_id);
    assert_eq!(
        registered[0].start_key.as_ref(),
        request(input.clone()).with_host_start_key(KEY).start_key(),
        "the retry registered under the host's key"
    );
    let referrers = core
        .backend()
        .attachment_referrers()
        .attachment_referrers(&input.id)
        .await
        .expect("the input's referrers read");
    assert!(
        referrers.contains(&lash_core::ArtifactReferrer::ProcessRecord(
            started.process_id.clone()
        )),
        "the retried start's process holds its input: {referrers:?}"
    );
    core.shutdown().await?;
    Ok(())
}
