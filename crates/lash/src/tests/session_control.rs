//! Laws of the resolved session-work port's control half (FIG-3871,
//! FIG-3849 F1): the facade's resolved port forwards
//! `SessionWorkEngine::control` to the deployment's engine, so a session
//! close's engine half releases a running root's execution instead of
//! answering `NothingHeld` while the turn runs on.
//!
//! It runs facade-built on lash-restate's engine over the Restate server
//! double — the engine whose release kills an invocation. On the bug the
//! port answers the trait default `NoEngineControl`: the close's
//! `release_root` reads `NothingHeld`, the intent is acknowledged anyway,
//! and the held model call runs on.

use super::*;

const SEED: u64 = 0xc047_0ff1;

/// What the law's held model call reports: `held` counts the call once it
/// blocks, `dropped` its drop — on Restate only the invocation's kill drops
/// a running call's task.
#[derive(Default)]
struct HeldCall {
    held: AtomicUsize,
    dropped: AtomicUsize,
}

/// Drops with the model call's future: the engine's release kills the
/// invocation, the server aborts its task, and the call's drop is the proof
/// the release ran.
struct HeldUntilKill(Arc<HeldCall>);

impl Drop for HeldUntilKill {
    fn drop(&mut self) {
        self.0.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

/// A model that answers every input but a `hold` one's: that call counts
/// itself held and never answers, keeping its root's invocation running.
fn hold_provider(calls: Arc<HeldCall>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("session-control-law")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            async move {
                let text = last_user_text(&request);
                if text.contains("hold") {
                    let _until_kill = HeldUntilKill(Arc::clone(&calls));
                    calls.held.fetch_add(1, Ordering::SeqCst);
                    std::future::pending::<()>().await;
                }
                Ok(text_response(&format!("echo: {text}")))
            }
        })
        .build()
        .into_handle()
}

/// A deletion's handler execution: the deployment's administration over the
/// handler's own controller, as a Restate deployment's delete endpoint runs
/// it.
struct HandlerExecution<'a> {
    admin: crate::SessionAdministration,
    scoped: lash_core::ScopedEffectController<'a>,
}

impl crate::SessionDeleteExecution for HandlerExecution<'_> {
    fn administration(&self) -> &crate::SessionAdministration {
        &self.admin
    }

    fn scoped<'run>(
        &'run self,
        _: lash_core::AdmittedScope,
    ) -> std::result::Result<lash_core::ScopedEffectController<'run>, lash_core::RuntimeError> {
        Ok(self.scoped.clone())
    }
}

/// Delete `session` inside a handler of the deployment's own host: the
/// close's recorded step runs under the handler's scoped controller.
async fn delete_in_handler(
    double: &lash_restate_test::RestateTestBackend,
    admin: crate::SessionAdministration,
    session: &SessionId,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<std::result::Result<(), String>>();
    let session = session.clone();
    double
        .run_in_handler(
            lash_core::AdmittedScope::session_delete(&session),
            Arc::new(move |scoped| {
                let execution = HandlerExecution {
                    admin: admin.clone(),
                    scoped,
                };
                let session = session.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let outcome = match crate::SessionDeleteContext::from_execution(
                        &execution,
                        session.as_str(),
                    ) {
                        Ok(context) => LashCore::delete_session(context)
                            .await
                            .map(|_| ())
                            .map_err(|error| error.to_string()),
                        Err(error) => Err(error.to_string()),
                    };
                    let _ = tx.send(outcome);
                })
            }),
        )
        .await
        .expect("the deletion's handler completed");
    rx.try_recv()
        .expect("the deletion's handler answered")
        .expect("the session was deleted");
}

/// A session close's engine half is the engine's own control (ADR 0104 O4):
/// `CloseSession`'s `release_root` reaches the deployment's engine through
/// the resolved port, so deleting a session whose root is held inside its
/// model call kills the running invocation — observed here by the held
/// call's drop.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_close_releases_its_running_roots_execution() -> Result<()> {
    let double = restate_double(SEED).await;
    let calls = Arc::new(HeldCall::default());
    let core = LashCore::standard_builder(double.lash_backend(), crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .provider(hold_provider(Arc::clone(&calls)))
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session("held-close").created().await.open().await?;
    let session_id = session.session_id().clone();
    session.send(TurnInput::text("hold this root")).await?;

    tokio::time::timeout(std::time::Duration::from_secs(60), async {
        while calls.held.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the sent root reached its held model call");

    delete_in_handler(&double, core.session_administration().await, &session_id).await;

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while calls.dropped.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the close's engine half released the running root's invocation");
    Ok(())
}
