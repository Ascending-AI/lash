use std::sync::{Arc, Mutex};

use lash::process::ProcessRegistry as _;

const SESSION: &str = "intent-ingress-observability-session";
const SCOPE: &str = "intent-ingress-observability-turn";
const PROCESS: &str = "intent-ingress-observability-process";

async fn test_core() -> lash::Result<lash::LashCore> {
    let registry = Arc::new(lash_core::TestLocalProcessRegistry::default());
    registry
        .register_process_with_observers(
            lash::process::ProcessRegistration::new(
                PROCESS,
                lash::process::ProcessInput::External {
                    metadata: serde_json::Value::Null,
                },
                lash::process::RecoveryContract::ExternallyOwned,
                lash::process::ProcessProvenance::host(),
            ),
            &[SESSION.to_string()],
        )
        .await?;
    let core = lash::LashCore::standard_builder(lash::TurnBudget::Unbounded)
        .provider(lash::provider::ProviderHandle::unconfigured())
        .model(
            lash::ModelSpec::builder("intent-ingress-observability-model")
                .context_window_tokens(4_096)
                .build()
                .expect("valid model"),
        )
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .attachment_store(Arc::new(lash::persistence::InMemoryAttachmentStore::new()))
        .process_env_store(Arc::new(
            lash::persistence::InMemoryProcessExecutionEnvStore::new(),
        ))
        .store_factory(Arc::new(
            lash::persistence::InMemorySessionStoreFactory::new(),
        ))
        .process_registry(registry)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "intent-ingress-observability-worker",
            "intent-ingress-observability-boot",
        ))?;
    let _session = core.session(SESSION).open().await?;
    Ok(core)
}

fn cancel_intent(session_id: &str) -> lash::tools::ToolIntent {
    lash::tools::ToolIntent::CancelProcess(lash::tools::CancelProcessIntent {
        session_id: session_id.to_string(),
        process_id: PROCESS.to_string(),
        reason: Some("observability-law".to_string()),
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
            let core = test_core().await?;
            let ingress =
                core.tool_intents(SESSION, lash::runtime::ExecutionScope::turn(SESSION, SCOPE))?;
            let key = ingress.key("observable", 0);
            assert!(matches!(
                ingress.submit(key.clone(), cancel_intent(SESSION)).await,
                lash::tools::ToolIntentIngressOutcome::Admitted { .. }
            ));
            assert!(matches!(
                ingress.submit(key, cancel_intent(SESSION)).await,
                lash::tools::ToolIntentIngressOutcome::Refused {
                    refusal: lash::tools::ToolIntentIngressRefusal::DuplicateIdentity { .. }
                }
            ));
            assert!(matches!(
                ingress
                    .submit(
                        ingress.key("observable-refused", 0),
                        cancel_intent("foreign")
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
        "replay_key=tool-intent:v1:sha256:",
        "decision=\"admitted\"",
        "decision=\"refused\"",
        "refusal_kind=\"duplicate_identity\"",
        "refusal_kind=\"intent_session_mismatch\"",
    ] {
        assert!(output.contains(field), "missing `{field}` in:\n{output}");
    }
    Ok(())
}
