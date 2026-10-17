//! SESSION-CANCEL-WITHOUT-PROVIDER (FIG-6072): a session whose claim
//! requirements no node serves is still cancellable. Its pending `Immediate`
//! turn cancel, or its close, reaches the session's terminal on a node that
//! serves none of them, and no provider is called on the way: neither the
//! tool the turn was running nor the model. An `AfterStep` cancel stays
//! pending until the host escalates it to `Immediate`.
//!
//! The requirement is the session's own (S7): the turn's admitted call
//! holds its provider's obligation token until it settles, and the
//! deployment that cancels it does not register that provider.
//!
//! Over SQLite memory; the PostgreSQL legs are ignored outside a PostgreSQL
//! gate, which selects them with `--include-ignored`.

use super::*;
use lash::{TurnCancelMode, TurnInput};
use lash_core::ToolDefinitionBindingExt as _;
use lash_core::facade_support::TurnCancelOutcome;

/// How long a stop may take to reach its terminal.
pub(super) const ENDS_WITHIN: std::time::Duration = std::time::Duration::from_secs(30);
/// How long a pending `AfterStep` cancel is watched for not ending.
const STAYS_PENDING_FOR: std::time::Duration = std::time::Duration::from_secs(2);
const TOOL: &str = "held_tool";

/// The stores a law's deployments run over, and what must outlive them.
async fn stores(
    storage: &str,
) -> (
    Arc<dyn lash_execution_contracts::backend::StoreSet>,
    Vec<Box<dyn std::any::Any + Send>>,
) {
    match storage {
        "sqlite_memory" => (sqlite_memory_store_set().await, Vec::new()),
        "postgres" => {
            let (stores, database, attachments) = postgres_store_parts().await;
            (stores, vec![Box::new(database), Box::new(attachments)])
        }
        other => panic!("no storage {other}"),
    }
}

/// What the law's providers were asked to do: each tool body and each model
/// call.
#[derive(Clone, Default)]
pub(super) struct Calls {
    tool: Arc<AtomicUsize>,
    model: Arc<AtomicUsize>,
    entered: Arc<tokio::sync::Notify>,
}

impl Calls {
    pub(super) fn counts(&self) -> (usize, usize) {
        (
            self.tool.load(Ordering::SeqCst),
            self.model.load(Ordering::SeqCst),
        )
    }
}

/// A tool whose body never answers.
#[derive(Clone)]
struct HeldTool(Calls);

fn tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        format!("tool:{TOOL}"),
        TOOL,
        "A call that never answers.",
        serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
        serde_json::json!({ "type": "object", "additionalProperties": true }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_core::ToolBinding::new(["tools"], TOOL))
}

#[async_trait]
impl ToolProvider for HeldTool {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<Vec<lash_core::ToolManifest>, lash::plugins::PluginError> {
        Ok(vec![tool_definition().manifest()])
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        (name == TOOL).then(|| Arc::new(tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.tool.fetch_add(1, Ordering::SeqCst);
        self.0.entered.notify_one();
        std::future::pending().await
    }
}

/// A model that calls [`TOOL`] and counts every call.
fn tool_calling_model(calls: &Calls) -> ProviderHandle {
    let model = Arc::clone(&calls.model);
    lash::testing::TestProvider::builder()
        .kind("cancel-without-provider")
        .complete(move |_request| {
            model.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: "call-1".to_string(),
                        tool_name: TOOL.to_string(),
                        input_json: "{}".to_string(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// The deployment that runs the turn, serving the held tool's provider.
fn deploy(backend: &lash_execution_contracts::backend::Backend, calls: &Calls) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(tool_calling_model(calls), mock_llm_profile_spec())
        .tools(
            lash::tool_provider_key!("tools"),
            Arc::new(HeldTool(calls.clone())),
        )
        .build(lash_facade_tests::runtime_lease_owner())
}

/// A deployment of the same model that registers no provider of the held
/// tool: it serves none of the stranded session's requirements.
pub(super) fn deploy_unserving(
    backend: &lash_execution_contracts::backend::Backend,
    calls: &Calls,
) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
        .serve_test_llm_profile(tool_calling_model(calls), mock_llm_profile_spec())
        .build(lash_facade_tests::runtime_lease_owner())
}

/// A session whose turn is in flight in its tool call when its node stops,
/// its admitted call holding the held tool's provider required; with the
/// provider calls counted when the node stopped.
pub(super) struct Stranded {
    pub(super) backend: lash_execution_contracts::backend::Backend,
    pub(super) calls: Calls,
    pub(super) before: (usize, usize),
}

impl Stranded {
    /// The stranded turn, attached through a deployment that serves none of
    /// the session's requirements.
    async fn turn(&self, core: &LashCore, id: &str) -> Result<lash::SendHandle> {
        let session = core.session(SessionId::fixture(id)).open().await?;
        Ok(session.attach_id(lash::TurnId::parse(RUN).expect("nonblank host identity")))
    }
}

const RUN: &str = "stranded-turn";

async fn stranded(
    storage: &str,
    id: &str,
) -> Result<(Stranded, Vec<Box<dyn std::any::Any + Send>>)> {
    stranded_over(storage, id, std::convert::identity).await
}

/// [`stranded`] over what `layer` makes of the storage's store set.
pub(super) async fn stranded_over(
    storage: &str,
    id: &str,
    layer: impl FnOnce(
        Arc<dyn lash_execution_contracts::backend::StoreSet>,
    ) -> Arc<dyn lash_execution_contracts::backend::StoreSet>
    + Send,
) -> Result<(Stranded, Vec<Box<dyn std::any::Any + Send>>)> {
    let (stores, held) = stores(storage).await;
    let backend = lash_conformance::backend_over(layer(stores));
    let calls = Calls::default();
    let core = deploy(&backend, &calls)?;
    let session = core
        .session(SessionId::fixture(id))
        .create(lash::SessionCreation::root(mock_session_spec()))
        .await?;
    let handle = session
        .send(TurnInput::text("use the tool"))
        .id(lash::TurnId::parse(RUN).expect("nonblank host identity"))
        .await?;
    calls.entered.notified().await;
    drop((handle, session));
    core.shutdown().await?;
    let before = calls.counts();
    Ok((
        Stranded {
            backend,
            calls,
            before,
        },
        held,
    ))
}

fn requested(receipt: lash::CancelReceipt) -> TurnCancelOutcome {
    match receipt {
        lash::CancelReceipt::Cancelled { receipt, .. } => receipt.outcome,
        other => panic!("the cancel addressed the open run: {other:?}"),
    }
}

/// An `Immediate` cancel of a session no node serves ends its turn on a node
/// that serves none of its requirements, without calling a provider.
async fn an_immediate_cancel_ends_an_unserved_session_without_a_provider(
    storage: &str,
) -> Result<()> {
    const ID: &str = "unserved-immediate-cancel";
    let (stranded, _held) = stranded(storage, ID).await?;
    let core = deploy_unserving(&stranded.backend, &stranded.calls)?;
    let turn = stranded.turn(&core, ID).await?;
    let outcome = requested(
        turn.cancel()
            .request_id("immediate-without-provider")
            .mode(TurnCancelMode::Immediate)
            .await?,
    );
    assert!(
        matches!(outcome, TurnCancelOutcome::Requested(_)),
        "{outcome:?}"
    );
    let output = tokio::time::timeout(ENDS_WITHIN, turn.output())
        .await
        .expect("the unserved session's cancelled turn reaches its terminal")?;
    let evidence = output
        .outcome
        .cancellation()
        .unwrap_or_else(|| panic!("the turn ended cancelled: {:?}", output.outcome));
    assert_eq!(evidence.request_id, "immediate-without-provider");
    assert_eq!(
        stranded.calls.counts(),
        stranded.before,
        "no tool body and no model call ran for the cancel"
    );
    core.shutdown().await?;
    Ok(())
}

/// The close of a session no node serves reaches its tombstone on a node
/// that serves none of its requirements, without calling a provider.
async fn a_close_ends_an_unserved_session_without_a_provider(storage: &str) -> Result<()> {
    const ID: &str = "unserved-close";
    let (stranded, _held) = stranded(storage, ID).await?;
    let core = deploy_unserving(&stranded.backend, &stranded.calls)?;
    let administration = core.session_administration().await;
    let deletion = LashCore::delete_session(administration.delete_context(ID)?).await?;
    assert!(
        matches!(deletion, lash::SessionDeletion::Requested { .. }),
        "{deletion:?}"
    );
    let completion = tokio::time::timeout(
        ENDS_WITHIN,
        core.await_session_deletion(&SessionId::fixture(ID)),
    )
    .await
    .expect("the unserved session's close reaches its tombstone")?;
    assert_eq!(completion, lash::SessionDeleteCompletion::Deleted);
    assert_eq!(
        stranded.calls.counts(),
        stranded.before,
        "no tool body and no model call ran for the close"
    );
    core.shutdown().await?;
    Ok(())
}

/// An `AfterStep` cancel of a session no node serves stays pending: no node
/// can finish the step it lets finish. Escalated to `Immediate`, it ends the
/// turn without calling a provider.
async fn an_after_step_cancel_of_an_unserved_session_waits_until_escalated(
    storage: &str,
) -> Result<()> {
    const ID: &str = "unserved-after-step-cancel";
    let (stranded, _held) = stranded(storage, ID).await?;
    let core = deploy_unserving(&stranded.backend, &stranded.calls)?;
    let turn = stranded.turn(&core, ID).await?;
    let outcome = requested(
        turn.cancel()
            .request_id("after-step-without-provider")
            .mode(TurnCancelMode::AfterStep)
            .await?,
    );
    assert!(
        matches!(outcome, TurnCancelOutcome::Requested(_)),
        "{outcome:?}"
    );
    assert!(
        tokio::time::timeout(STAYS_PENDING_FOR, stranded.turn(&core, ID).await?.output())
            .await
            .is_err(),
        "an after-step cancel no node can honour ended the turn"
    );
    let escalated = requested(
        turn.cancel()
            .request_id("escalated-without-provider")
            .mode(TurnCancelMode::Immediate)
            .await?,
    );
    assert!(
        matches!(escalated, TurnCancelOutcome::Escalated { .. }),
        "{escalated:?}"
    );
    let output = tokio::time::timeout(ENDS_WITHIN, turn.output())
        .await
        .expect("the escalated cancel reaches the turn's terminal")?;
    let evidence = output
        .outcome
        .cancellation()
        .unwrap_or_else(|| panic!("the turn ended cancelled: {:?}", output.outcome));
    assert_eq!(evidence.mode, TurnCancelMode::Immediate);
    assert_eq!(
        stranded.calls.counts(),
        stranded.before,
        "no tool body and no model call ran for the cancel"
    );
    core.shutdown().await?;
    Ok(())
}

macro_rules! session_cancel_without_provider_laws {
    ($($(#[$service:meta])* $storage:ident;)*) => {
        $(
            mod $storage {
                use super::*;

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn an_immediate_cancel_ends_an_unserved_session_without_a_provider()
                -> Result<()> {
                    super::an_immediate_cancel_ends_an_unserved_session_without_a_provider(
                        stringify!($storage),
                    )
                    .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn a_close_ends_an_unserved_session_without_a_provider() -> Result<()> {
                    super::a_close_ends_an_unserved_session_without_a_provider(
                        stringify!($storage),
                    )
                    .await
                }

                #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
                $(#[$service])*
                async fn an_after_step_cancel_of_an_unserved_session_waits_until_escalated()
                -> Result<()> {
                    super::an_after_step_cancel_of_an_unserved_session_waits_until_escalated(
                        stringify!($storage),
                    )
                    .await
                }
            }
        )*
    };
}

session_cancel_without_provider_laws! {
    sqlite_memory;
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a with-service.sh pg gate"]
    postgres;
}
