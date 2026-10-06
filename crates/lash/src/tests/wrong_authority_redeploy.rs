//! A deployment opened over existing stores under another Restate authority
//! answers its senders (FIG-4597).
//!
//! A session's first run records the cancellation binding of the authority
//! it ran under. A deployment that later runs over the same stores under
//! another authority presents another binding, and the store refuses it on
//! every attempt: `TurnCancelBindingMismatch`. The refusal is the run's
//! recorded answer, so the run ends with it and its sender reads the typed
//! cause. Retried as an attempt fault, the run paused after its attempt
//! budget and its sender waited on it forever.

use super::*;

const SEED: u64 = 0x4597_0001;
const OTHER_SEED: u64 = 0x4597_0002;

/// How long a run the deployment can never run may take to answer its
/// sender. The bound turns a sender left waiting on a paused run into a
/// failure.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

fn core_over(double: &lash_restate_test::RestateTestBackend) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
        .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())
}

/// A session executes a run under one authority. Another deployment then opens
/// the same stores under another authority, and each send to the session is
/// answered, within a bound, with the typed binding mismatch naming both
/// authorities. No run is left paused.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_under_another_authority_is_answered_with_the_binding_mismatch() -> Result<()> {
    const ID: &str = "wrong-authority-redeploy";
    let first = restate_double(SEED).await;
    {
        let core = core_over(&first)?;
        core.session(crate::SessionId::parse(ID).expect("nonblank host identity"))
            .created()
            .await
            .open()
            .await?;
        core.session(crate::SessionId::parse(ID).expect("nonblank host identity"))
            .durable()
            .await?
            .send(TurnInput::text("under the first authority"))
            .output()
            .await?;
        first
            .settle_session_shift(&lash_core::SessionId::from(ID))
            .await;
    }
    let stores = Arc::clone(first.engine_stores());
    drop(first);
    let second = lash_restate_test::backend_with(
        OTHER_SEED,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("another deployment over the same stores");
    let core = core_over(&second)?;
    // Twice: the refusal ends the run it met, so the next send is a new
    // run that is refused the same way rather than one queued behind it.
    for send in ["the first send", "the second send"] {
        let answer = tokio::time::timeout(ANSWERS_WITHIN, async {
            core.session(crate::SessionId::parse(ID).expect("nonblank host identity"))
                .durable()
                .await?
                .send(TurnInput::text("under another authority"))
                .output()
                .await
        })
        .await;
        let Ok(answer) = answer else {
            panic!(
                "{send}: a run refused for its session's cancellation binding answers its \
                 sender: none in {ANSWERS_WITHIN:?}, invocations {:?}",
                invocations(&second)
            );
        };
        let Err(EmbedError::Runtime(error)) = answer else {
            panic!("{send}: the sender reads the runtime refusal: {answer:?}");
        };
        assert_eq!(
            error.code,
            lash_core::RuntimeErrorCode::TurnCancelBindingMismatch,
            "{send}: {error:?}"
        );
        let Some(lash_core::RuntimeErrorCause::StoreRefusal { refusal }) = &error.cause else {
            panic!("{send}: the refusal carries its typed cause: {error:?}");
        };
        let lash_core::store::StoreRefusal::TurnCancelBindingMismatch {
            session_id,
            expected,
            presented,
        } = &**refusal
        else {
            panic!("{send}: the cause is the binding mismatch: {refusal:?}");
        };
        assert_eq!(session_id.as_str(), ID, "{send}");
        assert_ne!(
            expected, presented,
            "{send}: the cause names the admitted and the presented authority"
        );
        assert!(
            error.is_terminal() && !error.is_retryable(),
            "{send}: {error:?}"
        );
    }
    let paused = invocations(&second)
        .into_iter()
        .filter(|invocation| invocation.contains(" paused "))
        .collect::<Vec<_>>();
    assert!(
        paused.is_empty(),
        "no refused run is left paused: {paused:?}"
    );
    Ok(())
}

mod permanent_run_admission {
    use super::*;
    use lash_core::store::{RuntimeStoreDecorator, ShiftAdmissionReceipt, ShiftAdmissionWrite};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// Refuses the atomic root admission (FIG-4848), which selects, seals,
    /// binds cancellation authority and records the run in one transaction,
    /// with the canonical writer-fence refusal once armed.
    struct AdmissionRefusalStore {
        inner: Arc<dyn lash_core::DeploymentStore>,
        armed: AtomicBool,
        refusals: AtomicUsize,
    }

    #[async_trait]
    impl RuntimeStoreDecorator for AdmissionRefusalStore {
        type Inner = dyn lash_core::DeploymentStore;

        fn inner(&self) -> &Self::Inner {
            self.inner.as_ref()
        }

        async fn commit_shift_admission(
            &self,
            request: &ShiftAdmissionWrite,
            anchor: &lash_trace::TraceAnchor,
        ) -> std::result::Result<ShiftAdmissionReceipt, StoreError> {
            if self.armed.load(Ordering::SeqCst) {
                self.refusals.fetch_add(1, Ordering::SeqCst);
                lash_core::store::FleetFormat::fence(
                    2,
                    lash_core::compat::VersionRange::exactly(1),
                )?;
            }
            self.inner.commit_shift_admission(request, anchor).await
        }
    }

    impl lash_core::DeploymentStoreDecorator for AdmissionRefusalStore {}

    async fn refusal_reaches_sender(postgres: bool) {
        let (stores, _held): (Arc<dyn lash_core::StoreSet>, Box<dyn std::any::Any>) = if postgres {
            postgres_store_set().await.expect("PostgreSQL gate")
        } else {
            let files = tempfile::tempdir().expect("SQLite store directory");
            let stores = lash_sqlite_store::SqliteStoreSet::open(files.path())
                .await
                .expect("SQLite file stores");
            (Arc::new(stores), Box::new(files))
        };
        let double = lash_restate_test::backend_with(
            SEED + 0x100,
            lash_restate_test::ServerConfig::default(),
            move |_| stores,
        )
        .await
        .expect("the double over the admission-law stores");
        let store = Arc::new(AdmissionRefusalStore {
            inner: double.lash_backend().session_store_factory(),
            armed: AtomicBool::new(false),
            refusals: AtomicUsize::new(0),
        });
        let backend = DecoratedBackend::over(double.lash_backend()).session_store_factory({
            let store = Arc::clone(&store);
            move |_| store
        });
        let core = explicit_ephemeral_facets(LashCore::standard_builder(backend.into()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec())
            .build(crate::testing::runtime_lease_owner())
            .expect("the core over the fenced admission store");
        const ID: &str = "permanent-run-admission";
        create_catalog_session(&core, ID)
            .await
            .expect("create the session");
        let durable = core
            .session(crate::SessionId::parse(ID).expect("nonblank host identity"))
            .durable()
            .await
            .expect("open before fencing admission");
        durable
            .pending_turn_inputs()
            .await
            .expect("resolve before fencing admission");
        store.armed.store(true, Ordering::SeqCst);
        let answer = tokio::time::timeout(
            ANSWERS_WITHIN,
            durable
                .send(TurnInput::text("a permanently refused run"))
                .output(),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the sender must receive the refusal, {:?}",
                invocations(&double)
            )
        });
        let Err(EmbedError::Runtime(error)) = answer else {
            panic!("the sender receives the typed runtime refusal: {answer:?}");
        };
        let expected = lash_core::store::StoreRefusal::WriterFenced {
            recorded: 2,
            writable: lash_core::compat::VersionRange::exactly(1),
        };
        let (code, cause) = (
            expected.code(),
            serde_json::json!({ "kind": "store_refusal", "refusal": expected }),
        );
        assert_eq!(error.code, code);
        assert_eq!(serde_json::to_value(&error.cause).unwrap(), cause);
        assert!(error.is_terminal() && !error.is_retryable());
        tokio::time::timeout(
            ANSWERS_WITHIN,
            double.settle_session_shift(&SessionId::from(ID)),
        )
        .await
        .expect("the refused run and its shift finish");
        assert_eq!(
            store.refusals.load(Ordering::SeqCst),
            1,
            "the permanent refusal is never retried"
        );
        let server = double.server();
        let runs: Vec<_> = server
            .invocations()
            .into_iter()
            .filter(|view| view.target.starts_with("LashTurn") && view.target.ends_with("/run"))
            .collect();
        assert_eq!(runs.len(), 1, "one refused run: {runs:?}");
        let run = &runs[0];
        assert_eq!(run.retry_count, 0, "{run:?}");
        assert_ne!(run.status, "paused");
        let recorded: Vec<serde_json::Value> = server
            .journal(&run.id)
            .expect("the run's retained journal")
            .iter()
            .filter_map(|entry| entry.run_completion()?.ok())
            .filter_map(|bytes| serde_json::from_slice(&bytes).ok())
            .filter(|value: &serde_json::Value| value["outcome"]["Err"]["code"] == code.as_str())
            .collect();
        assert_eq!(
            recorded.len(),
            1,
            "the admission records one typed refusal: {recorded:?}"
        );
        assert_eq!(recorded[0]["outcome"]["Err"]["cause"], cause);
    }

    macro_rules! laws {
        ($module:ident, $postgres:expr $(, $ignore:meta)?) => {
            mod $module {
                use super::*;
                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$ignore])?
                async fn an_admission_writer_refusal_is_recorded_and_reaches_the_sender() {
                    refusal_reaches_sender($postgres).await;
                }
            }
        };
    }

    laws!(sqlite_file, false);
    laws!(postgres, true, ignore = "requires the PostgreSQL gate");
}
