#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the setup helpers around them in this target are test code too"
)]

use lash_core::ProcessRegistrar as _;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;
use std::sync::{Arc, Mutex};

const SESSION: &str = "intent-ingress-observability-session";
const SCOPE: &str = "intent-ingress-observability-turn";
/// The core, and its two fixture processes: the cancel target, and a second
/// target, so an identity re-used for a different cancel is the changed
/// payload the submission ledger refuses.
async fn test_core() -> lash::Result<(lash::LashCore, ProcessId, ProcessId)> {
    let backend = lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a memory backend");
    let registry = backend.process_registry();
    let mut targets = Vec::new();
    for _ in 0..2 {
        let process = registry
            .register_process_with_observers(
                lash::process::ProcessRegistration::new(
                    lash::process::ProcessInput::External {
                        metadata: serde_json::Value::Null,
                    },
                    lash::process::RecoveryContract::ExternallyOwned,
                    lash::process::ProcessProvenance::host(),
                    lash_core::Lifetime::Detached,
                ),
                &[SessionId::from(SESSION.to_string())],
            )
            .await?
            .id;
        targets.push(process);
    }
    let [process, other_process] = <[ProcessId; 2]>::try_from(targets).expect("two targets");
    let core =
        lash::LashCore::standard_builder(Arc::new(backend).into(), lash::TurnBudget::Unbounded)
            .provider(lash::provider::ProviderHandle::unconfigured())
            .model(
                lash::ModelSpec::builder("intent-ingress-observability-model")
                    .context_window_tokens(4_096)
                    .build()
                    .expect("valid model"),
            )
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
            .build(lash::persistence::LeaseOwnerIdentity::opaque(
                "intent-ingress-observability-worker",
                "intent-ingress-observability-boot",
            ))?;
    let _session = core.session(SESSION).open().await?;
    Ok((core, process, other_process))
}

fn cancel_intent_for(session_id: &SessionId, process: &ProcessId) -> lash::tools::ToolIntent {
    lash::tools::ToolIntent::CancelProcess(lash::tools::CancelProcessIntent {
        session_id: SessionId::from(session_id.to_string()),
        process_id: process.clone(),
    })
}

#[derive(Clone)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for Capture {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

#[test]
fn ingress_records_identity_and_every_decision_class() -> lash::Result<()> {
    // With at most one dispatcher registered, `tracing` rebuilds interest
    // from the calling thread's default, so a sibling test on a defaultless
    // thread registering a new callsite can turn every callsite off while
    // this capture is installed. One extra dispatcher kept alive for the
    // process makes rebuilds consult the live dispatchers instead (the same
    // pin `lash_core::testing`'s trace capture holds).
    static PIN: std::sync::Once = std::sync::Once::new();
    PIN.call_once(|| std::mem::forget(tracing::Dispatch::new(tracing_subscriber::registry())));
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::INFO)
        .with_writer(Capture(Arc::clone(&bytes)))
        .finish();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("observability law runtime");

    tracing::subscriber::with_default(subscriber, || {
        runtime.block_on(async {
            let (core, process, other_process) = test_core().await?;
            let ingress =
                core.tool_intents(SESSION, lash::runtime::ExecutionScope::turn(SESSION, SCOPE))?;
            let key = ingress.key("observable", 0);
            assert!(matches!(
                ingress
                    .submit(
                        key.clone(),
                        cancel_intent_for(&SessionId::from(SESSION), &process)
                    )
                    .await,
                lash::tools::ToolIntentIngressOutcome::Admitted { .. }
            ));
            // The backend's host journals the first submission: the same
            // identity naming a different target is the changed payload the
            // submission ledger refuses.
            assert!(matches!(
                ingress
                    .submit(
                        key,
                        cancel_intent_for(&SessionId::from(SESSION), &other_process)
                    )
                    .await,
                lash::tools::ToolIntentIngressOutcome::Refused {
                    refusal: lash::tools::ToolIntentIngressRefusal::DuplicateIdentity { .. }
                }
            ));
            assert!(matches!(
                ingress
                    .submit(
                        ingress.key("observable-refused", 0),
                        cancel_intent_for(&SessionId::from("foreign"), &process)
                    )
                    .await,
                lash::tools::ToolIntentIngressOutcome::Refused {
                    refusal: lash::tools::ToolIntentIngressRefusal::IntentSessionMismatch { .. }
                }
            ));
            Ok::<(), lash::EmbedError>(())
        })
    })?;

    let output = String::from_utf8(
        bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone(),
    )
    .expect("tracing formatter emits UTF-8");
    for field in [
        "tool_intent_ingress.submit",
        "session_id=intent-ingress-observability-session",
        "execution_scope_id=intent-ingress-observability-turn",
        "tool_call_id=observable",
        "intent_index=0",
        "replay_key=tool-intent:v2:blake3:",
        "decision=\"admitted\"",
        "decision=\"refused\"",
        "refusal_kind=\"duplicate_identity\"",
        "refusal_kind=\"intent_session_mismatch\"",
    ] {
        assert!(output.contains(field), "missing `{field}` in:\n{output}");
    }
    Ok(())
}
