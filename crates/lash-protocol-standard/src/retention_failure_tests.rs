//! A tool result's presentation retains an oversized output through the
//! attachment store: a permanent refusal of that put is a terminal, typed
//! failure that never retries the put, and a transient fault of a
//! presentation step never becomes recorded text. Each law runs
//! over SQLite memory, a SQLite file and PostgreSQL; the session actor's next
//! pass is what reruns a presentation, so a rerun here is a second call.

use lash_core::facade_support::*;
use lash_core::*;
use lash_sansio::core_support::ModelToolReturnCoreSupport as _;
use std::sync::Arc;

const CALL_ID: &str = "oversized-call";
const LIVE_POLICY: OutputRetentionPolicy = OutputRetentionPolicy {
    inline_limit_bytes: 1024,
    witness_bytes: 256,
};
fn message() -> String {
    "a stack frame of the failed call\n".repeat(600)
}

async fn present_with(
    backend: &Backend,
    attachments: Arc<RuntimeAttachmentStore>,
    steps: Vec<lash_core::plugin::ToolPresentationStep>,
) -> Result<ModelToolReturn, RuntimeEffectControllerError> {
    let mut factories = lash_core::testing::test_code_protocol_factories();
    let mut spec = lash_core::plugin::PluginSpec::new();
    for step in steps {
        spec = spec.with_presentation_step(lash_core::hook_key!("presentation-step-1"), step);
    }
    factories.push(Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("retention-law"),
        spec,
    )));
    let context = lash_core::testing::TestExecutionContextBuilder::for_backend(backend)
        .session_id("retention-law-session")
        .plugin_factories(factories)
        .attachment_store(attachments)
        .build()
        .into_runtime();
    lash_core::testing::runtime_internals::complete_tool_output(
        &context,
        ToolCallId::fixture(CALL_ID),
        "oversized",
        ToolCallOutput::success_tool_value(ToolValue::String(message())),
    )
    .await
}

async fn permanent_retention_refusals_settle(backend: Backend, ended: bool) {
    let process = ProcessId::fixture("retention-ended-process");
    if ended {
        backend
            .attachment_referrers()
            .end_attachment_referrer(&ArtifactReferrer::ProcessRecord(process.clone()))
            .await
            .expect("end the referrer");
    }
    let attachments = Arc::new(
        if ended {
            RuntimeAttachmentStore::new(
                backend.attachment_store(),
                backend.attachment_referrers(),
                RuntimeOwner::Process(process),
                AttachmentPolicy::standard(),
            )
        } else {
            RuntimeAttachmentStore::ephemeral(
                backend.attachment_store(),
                AttachmentPolicy::standard(),
            )
            .with_max_attachment_bytes(Some(64))
        }
        .with_output_retention(LIVE_POLICY),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let step: lash_core::plugin::ToolPresentationStep = {
        let calls = Arc::clone(&calls);
        Arc::new(move |input| {
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                // Catching a refused put must not turn it into presentation text.
                let _ = input.context.artifacts.retain_text("law", &message()).await;
                Ok(ModelToolReturn::text(
                    input.context.tool_name,
                    "caught refusal",
                ))
            })
        })
    };
    let first = present_with(&backend, attachments, vec![step])
        .await
        .expect_err("a permanent put refusal fails presentation");

    assert!(
        first.is_terminal(),
        "permanent retention is terminal: {first:?}"
    );
    assert!(
        !first
            .journal_disposition(RuntimeEffectKind::PresentToolResult)
            .is_retryable_derivation()
    );
    assert!(
        first.cause.is_some(),
        "the attachment-store cause stays typed"
    );
    let evidence = serde_json::to_value(&first).expect("typed refusal evidence");
    assert_eq!(evidence["cause"]["kind"], "attachment_retention");
    if ended {
        assert_eq!(
            evidence["cause"]["failure"]["kind"],
            "referrers_operation_failed"
        );
        assert_eq!(
            evidence["cause"]["failure"]["source"]["cause"]["kind"],
            "artifact_referrer_ended"
        );
    } else {
        assert_eq!(evidence["cause"]["failure"]["kind"], "size_limit_exceeded");
        assert_eq!(evidence["cause"]["failure"]["byte_len"], message().len());
        assert_eq!(evidence["cause"]["failure"]["max_bytes"], 64);
    }
    let wire = serde_json::to_vec(&first).expect("encode the typed refusal");
    let decoded: RuntimeEffectControllerError =
        serde_json::from_slice(&wire).expect("decode refusal");
    assert_eq!(decoded.cause, first.cause);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "a terminal refusal never retries its put"
    );
}

async fn transient_step_failure_never_becomes_text(backend: Backend) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let step: lash_core::plugin::ToolPresentationStep = {
        let calls = Arc::clone(&calls);
        Arc::new(move |input| {
            let fail = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0;
            Box::pin(async move {
                if fail {
                    return Err(RuntimeEffectControllerError::new(
                        RuntimeErrorCode::RecordedRendererUnavailable,
                        "transient step fault",
                    )
                    .retryable_uncommitted_derivation()
                    .into());
                }
                Ok(ModelToolReturn::text(
                    input.context.tool_name,
                    "recovered presentation",
                ))
            })
        })
    };
    let attachments = || {
        Arc::new(RuntimeAttachmentStore::ephemeral(
            backend.attachment_store(),
            AttachmentPolicy::standard(),
        ))
    };

    let failed = present_with(&backend, attachments(), vec![Arc::clone(&step)])
        .await
        .expect_err("the transient fault fails the pass");
    assert!(
        failed
            .journal_disposition(RuntimeEffectKind::PresentToolResult)
            .is_retryable_derivation(),
        "a transient step fault is retried, never recorded: {failed:?}"
    );
    let recovered = present_with(&backend, attachments(), vec![step])
        .await
        .expect("the rerun reruns the failed step");
    assert_eq!(
        recovered.parts,
        vec![ModelToolReturnPart::text("recovered presentation")],
        "a transient step fault never becomes recorded text",
    );
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "only the failed derivation is retried"
    );
}

/// A fresh store set of each tier, and what must outlive it.
async fn sqlite_memory() -> (Backend, Box<dyn std::any::Any>) {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite memory stores"),
    );
    (lash_conformance::backend_over(stores), Box::new(()))
}

async fn sqlite_file() -> (Backend, Box<dyn std::any::Any>) {
    let dir = tempfile::tempdir().expect("SQLite store directory");
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::open(
            dir.path().join("lash.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("SQLite file stores"),
    );
    (lash_conformance::backend_over(stores), Box::new(dir))
}

#[allow(
    clippy::disallowed_methods,
    reason = "the test host reads the PostgreSQL service URL its gate sets"
)]
async fn postgres() -> (Backend, Box<dyn std::any::Any>) {
    let url = lash_postgres_store::testing::required_database_url();
    let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
    let storage = lash_postgres_store::testing::connect(database.url())
        .await
        .expect("connect PostgreSQL");
    let attachments = tempfile::tempdir().expect("PostgreSQL attachment directory");
    let stores = Arc::new(lash_postgres_store::PostgresStoreSet::new(
        &storage,
        lash_sqlite_store::SqliteStoreSet::open(
            (attachments.path()).join("attachments.db"),
            lash_sqlite_store::SqliteSynchronous::Normal,
        )
        .await
        .expect("SQLite attachment store")
        .attachment_store(),
    ));
    (
        lash_conformance::backend_over(stores),
        Box::new((database, attachments, storage)),
    )
}

macro_rules! retention_laws {
    ($tier:ident $(, #[$service:meta])?) => {
        mod $tier {
            #[tokio::test]
            $(#[$service])?
            async fn permanent_retention_refusals_settle() {
                let (backend, _held) = super::$tier().await;
                super::permanent_retention_refusals_settle(backend, false).await;
            }

            #[tokio::test]
            $(#[$service])?
            async fn transient_step_failure_never_becomes_text() {
                let (backend, _held) = super::$tier().await;
                super::transient_step_failure_never_becomes_text(backend).await;
            }

            #[tokio::test]
            $(#[$service])?
            async fn ended_referrer_retention_settles() {
                let (backend, _held) = super::$tier().await;
                super::permanent_retention_refusals_settle(backend, true).await;
            }
        }
    };
}

retention_laws!(sqlite_memory);
retention_laws!(sqlite_file);
retention_laws!(
    postgres,
    #[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
);
