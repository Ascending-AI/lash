//! A send to a session the engine cannot open is answered, never left
//! waiting (FIG-4597).
//!
//! The engine opens a session's runtime for every drive
//! (`core/session_driver.rs`, `open_runtime`). Each way that open ends
//! terminally answers the sender of the input the drive was for, within a
//! bound, with the error that names the cause:
//!
//! - no catalog row, or a deleted one: the facade refuses the send before
//!   anything is accepted, as `UnknownSession` or the store's
//!   `SessionDeleted`;
//! - a catalog row with no head: `SessionCreationUnrecorded` (FIG-4553);
//! - a catalog read the store refuses with a typed refusal: that refusal's
//!   own code and cause;
//! - a catalog read the store refuses otherwise, and a plugin that refuses
//!   to build: `PluginSessionManager`, naming the refusal.
//!
//! A tool source lost under `ToolSourcePolicy::Require` is the runtime
//! build's own refusal; `tool_restore_report.rs` holds its law. A deployment
//! under another Restate authority opens and is refused at its root's
//! admission; `wrong_authority_redeploy.rs` holds that law.

use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

const SEED: u64 = 0x4597_0101;

/// How long a send to a session that cannot open may take to answer. The
/// bound turns a sender left waiting into a failure.
const ANSWERS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

fn builder(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .serve_test_model(mock_provider(), mock_model_spec())
}

/// The error a send to `id` is answered with, within [`ANSWERS_WITHIN`].
async fn refusal_of(
    core: &LashCore,
    double: &lash_restate_test::RestateTestBackend,
    id: &str,
) -> EmbedError {
    let sent = tokio::time::timeout(ANSWERS_WITHIN, async {
        core.session(id)
            .durable()
            .await?
            .send(TurnInput::text("to a session that cannot open"))
            .output()
            .await
    })
    .await;
    let answer = match sent {
        Ok(answer) => answer,
        Err(_) => panic!(
            "{id}: the send is answered: nothing in {ANSWERS_WITHIN:?}, invocations {:?}",
            invocations(double)
        ),
    };
    let error = match answer {
        Ok(output) => panic!("{id}: the session cannot open, got {:?}", output.result),
        Err(error) => error,
    };
    assert_nothing_paused(double);
    error
}

/// No invocation is left paused: the refusal ended what met it.
fn assert_nothing_paused(double: &lash_restate_test::RestateTestBackend) {
    let paused = invocations(double)
        .into_iter()
        .filter(|invocation| invocation.contains(" paused "))
        .collect::<Vec<_>>();
    assert!(paused.is_empty(), "nothing is left paused: {paused:?}");
}

/// The runtime error the engine's drive refused the send with.
fn drive_refusal(id: &str, error: EmbedError) -> lash_core::RuntimeError {
    match error {
        EmbedError::Runtime(error) => error,
        other => panic!("{id}: the drive's refusal is a runtime error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_an_unknown_session_is_answered_unknown() -> Result<()> {
    const ID: &str = "never-created";
    let double = restate_double(SEED).await;
    let core = builder(double.lash_backend()).build(crate::testing::runtime_lease_owner())?;
    let error = refusal_of(&core, &double, ID).await;
    assert!(
        matches!(&error, EmbedError::UnknownSession { session_id } if session_id.as_str() == ID),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_deleted_session_is_answered_deleted() -> Result<()> {
    const ID: &str = "deleted-before-the-send";
    let double = restate_double(SEED).await;
    let core = builder(double.lash_backend()).build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, ID).await?;
    lash_core::SessionCatalogStore::delete_session(
        core.store_factory.as_ref(),
        &SessionId::from(ID),
    )
    .await
    .expect("delete the catalog session");
    let error = refusal_of(&core, &double, ID).await;
    assert!(
        matches!(
            &error,
            EmbedError::Store(StoreError::SessionDeleted { session_id }) if session_id.as_str() == ID
        ),
        "{error:?}"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_catalog_row_with_no_head_is_answered_creation_unrecorded() -> Result<()> {
    const ID: &str = "row-with-no-head";
    let double = restate_double(SEED).await;
    let core = builder(double.lash_backend()).build(crate::testing::runtime_lease_owner())?;
    lash_core::runtime::admit_session_view(
        &core.store_factory,
        &lash_core::SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: SessionId::from(ID),
            relation: lash_core::SessionRelation::Root,
            config: lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded).into(),
            head: lash_core::SessionCreationHead::CommittedByCreator,
        },
    )
    .await
    .map_err(EmbedError::Store)?;
    let error = drive_refusal(ID, refusal_of(&core, &double, ID).await);
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::SessionCreationUnrecorded,
        "{error:?}"
    );
    assert!(error.is_terminal(), "{error:?}");
    Ok(())
}

/// A plugin factory that builds until `refuse` is set.
struct RefusingFactory {
    refuse: Arc<AtomicBool>,
}

impl lash_core::facade_support::PluginFactory for RefusingFactory {
    fn id(&self) -> &'static str {
        "fig4597-refusing-factory"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        if self.refuse.load(Ordering::SeqCst) {
            return Err(lash_core::PluginError::Session(
                "the plugin refuses to build".to_string(),
            ));
        }
        Ok(Arc::new(InertPlugin))
    }
}

struct InertPlugin;

impl lash_core::facade_support::SessionPlugin for InertPlugin {
    fn id(&self) -> &'static str {
        "fig4597-refusing-factory"
    }

    fn register(
        &self,
        _reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_to_a_session_whose_plugin_refuses_to_build_is_answered_with_the_refusal()
-> Result<()> {
    const ID: &str = "plugin-refuses";
    let double = restate_double(SEED).await;
    let refuse = Arc::new(AtomicBool::new(false));
    let core = builder(double.lash_backend())
        .plugin(Arc::new(RefusingFactory {
            refuse: Arc::clone(&refuse),
        }))
        .build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, ID).await?;
    refuse.store(true, Ordering::SeqCst);
    let error = drive_refusal(ID, refusal_of(&core, &double, ID).await);
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::PluginSessionManager,
        "{error:?}"
    );
    assert!(
        error.message.contains("the plugin refuses to build"),
        "the refusal names the plugin's own: {error:?}"
    );
    Ok(())
}

/// How the catalog refuses the engine's lookup.
#[derive(Clone, Copy, Debug)]
enum CatalogRefusal {
    /// A refusal with a typed carrier past the store.
    WriterFenced,
    /// A refusal the store states and nothing carries typed.
    Unsupported,
}

impl CatalogRefusal {
    fn error(self) -> StoreError {
        match self {
            Self::WriterFenced => StoreError::WriterFenced {
                recorded: 2,
                writable: lash_core::compat::VersionRange::exactly(1),
            },
            Self::Unsupported => StoreError::UnsupportedStoreOperation {
                operation: "lookup_session",
            },
        }
    }
}

/// A catalog whose lookup refuses once `refuse` is set.
struct RefusingCatalog {
    inner: Arc<dyn DeploymentStore>,
    refusal: CatalogRefusal,
    refuse: AtomicBool,
}

#[async_trait]
impl lash_core::store::RuntimeStoreDecorator for RefusingCatalog {
    type Inner = dyn DeploymentStore;

    fn inner(&self) -> &Self::Inner {
        self.inner.as_ref()
    }

    async fn lookup_session(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<lash_core::store::SessionLookup, StoreError> {
        if self.refuse.load(Ordering::SeqCst) {
            Err(self.refusal.error())
        } else {
            self.inner.lookup_session(session_id).await
        }
    }
}

impl lash_core::DeploymentStoreDecorator for RefusingCatalog {}

/// The send is accepted, and then the catalog refuses the lookup the
/// engine's open makes: the sender is answered with the drive's refusal.
async fn a_send_whose_drive_meets_a_refusing_catalog_is_answered(
    refusal: CatalogRefusal,
) -> Result<lash_core::RuntimeError> {
    let id = format!("catalog-refuses-{refusal:?}").to_lowercase();
    let double = restate_double(SEED).await;
    let catalog = Arc::new(RefusingCatalog {
        inner: double.lash_backend().session_store_factory(),
        refusal,
        refuse: AtomicBool::new(false),
    });
    let backend = DecoratedBackend::over(double.lash_backend()).session_store_factory({
        let catalog = Arc::clone(&catalog);
        move |_| catalog
    });
    let core = builder(backend.into()).build(crate::testing::runtime_lease_owner())?;
    crate::tests::create_catalog_session(&core, &id).await?;
    let durable = core.session(id.as_str()).durable().await?;
    // The catalog refuses from before the send, so no drive opens the
    // session ahead of the refusal; the facade's own acquisition was made
    // above.
    durable.pending_turn_inputs().await?;
    catalog.refuse.store(true, Ordering::SeqCst);
    let answer = tokio::time::timeout(ANSWERS_WITHIN, async {
        durable
            .send(TurnInput::text("accepted before the engine's open"))
            .output()
            .await
    })
    .await;
    let Ok(answer) = answer else {
        panic!(
            "{id}: the send is answered: nothing in {ANSWERS_WITHIN:?}, invocations {:?}",
            invocations(&double)
        );
    };
    let error = match answer {
        Ok(output) => panic!("{id}: the session cannot open, got {:?}", output.result),
        Err(error) => drive_refusal(&id, error),
    };
    assert_nothing_paused(&double);
    Ok(error)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_whose_open_meets_a_typed_store_refusal_is_answered_with_it() -> Result<()> {
    let error =
        a_send_whose_drive_meets_a_refusing_catalog_is_answered(CatalogRefusal::WriterFenced)
            .await?;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::WriterFenced,
        "{error:?}"
    );
    let Some(lash_core::RuntimeErrorCause::StoreRefusal { refusal }) = &error.cause else {
        panic!("the refusal carries its typed cause: {error:?}");
    };
    assert_eq!(
        refusal.clone().into_store_error().variant_name(),
        CatalogRefusal::WriterFenced.error().variant_name()
    );
    assert!(error.is_terminal(), "{error:?}");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_send_whose_open_meets_an_untyped_store_refusal_is_answered_naming_it() -> Result<()> {
    let error =
        a_send_whose_drive_meets_a_refusing_catalog_is_answered(CatalogRefusal::Unsupported)
            .await?;
    assert_eq!(
        error.code,
        lash_core::RuntimeErrorCode::PluginSessionManager,
        "{error:?}"
    );
    assert!(
        error.message.contains("lookup_session"),
        "the refusal names the store's own: {error:?}"
    );
    Ok(())
}
