//! Mutations of the shared losing-wait isolation law over the server double.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::testing::{EffectLayer, LayeredEffectHost};
use lash_core::{
    AdmittedScope, AwaitEventKey, AwaitEventResolver, EffectGroupHandle, EffectHost, LoserPolicy,
    RuntimeEffectController, RuntimeEffectControllerError, RuntimeError, RuntimeErrorCode,
    ScopedEffectController, SessionId,
};
use std::future::Future;
use std::task::Poll;
use tokio::sync::{Notify, oneshot};

use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use crate::durable_wait::{arm_wait_registration_witness, hold_wait_registration};

#[derive(Clone, Copy)]
enum Listing {
    InitialError,
    PollError,
    Unsupported,
}

struct ListingHost {
    inner: Arc<dyn EffectHost>,
    listing: Listing,
    reads: AtomicUsize,
}

impl AwaitEventResolver for ListingHost {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }
}

#[async_trait::async_trait]
impl EffectHost for ListingHost {
    fn await_event_resolver(&self) -> &dyn AwaitEventResolver {
        self.inner.await_event_resolver()
    }

    async fn drain_usage_accounting(
        &self,
        owner: &lash_core::RuntimeOwner,
    ) -> Result<lash_core::UsageOwnerRetired, RuntimeError> {
        self.inner.drain_usage_accounting(owner).await
    }

    async fn retire_usage_execution(
        &self,
        owner: &lash_core::RuntimeOwner,
        scope: &lash_core::ExecutionScope,
    ) -> Result<u64, RuntimeError> {
        self.inner.retire_usage_execution(owner, scope).await
    }

    async fn journal_replay(
        &self,
        journal: &lash_sansio::EffectJournalIdentity,
    ) -> Result<lash_core::JournalReplay, RuntimeError> {
        self.inner.journal_replay(journal).await
    }

    fn turn_control_binding_id(&self) -> String {
        self.inner.turn_control_binding_id()
    }

    fn scoped<'run>(
        &'run self,
        admitted: AdmittedScope,
    ) -> Result<ScopedEffectController<'run>, RuntimeError> {
        self.inner.scoped(admitted)
    }

    async fn list_outstanding_await_event_keys(
        &self,
        session: &SessionId,
    ) -> Result<Vec<AwaitEventKey>, RuntimeError> {
        let read = self.reads.fetch_add(1, Ordering::SeqCst);
        match self.listing {
            Listing::InitialError if read == 0 => Err(RuntimeError::new(
                RuntimeErrorCode::EngineAwaitEventPeek,
                "injected initial registry failure",
            )),
            Listing::PollError if read == 0 => Ok(Vec::new()),
            Listing::PollError => Err(RuntimeError::new(
                RuntimeErrorCode::EngineAwaitEventPeek,
                "injected subsequent registry failure",
            )),
            Listing::Unsupported => Err(RuntimeError::new(
                RuntimeErrorCode::AwaitEventUnsupported,
                "fixture explicitly does not list waits",
            )),
            Listing::InitialError => self.inner.list_outstanding_await_event_keys(session).await,
        }
    }
}

#[derive(Default)]
struct CloseMutation {
    closes: AtomicUsize,
    started: Notify,
    swept: Notify,
    cancel_session: Option<SessionId>,
    host: Option<Arc<dyn EffectHost>>,
}

#[async_trait::async_trait]
impl EffectLayer for CloseMutation {
    async fn close_effect_group(
        &self,
        inner: &dyn RuntimeEffectController,
        handle: EffectGroupHandle,
        disposition: LoserPolicy,
    ) -> Result<(), RuntimeEffectControllerError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        // Seat the scoped cancellation before sweeping sibling waits. A sweep
        // first can commit the loser before close and mask the isolation defect.
        inner.close_effect_group(handle, disposition).await?;
        if let (Some(host), Some(session)) = (&self.host, &self.cancel_session) {
            host.cancel_await_events_for_session(session)
                .await
                .expect("the mutation cancels every registered session wait");
            self.swept.notify_one();
        }
        Ok(())
    }
}

async fn run_mutation(listing: Listing, hold: bool, overbroad: bool, witness: bool) {
    let harness = LiveConformanceHarness::start_on(HarnessServer::in_process()).await;
    let factory = harness.group_host_factory();
    let inner = factory(Some(
        lash_conformance::registration_macro_support::effect_group_suite_executors(),
    ));
    let prefix = format!("wait-isolation-{}", uuid::Uuid::new_v4().simple());
    let mutation = Arc::new(CloseMutation {
        cancel_session: overbroad
            .then(|| SessionId::fixture(format!("{prefix}-losing-wait-session"))),
        host: overbroad.then(|| Arc::clone(&inner)),
        ..Default::default()
    });
    let host: Arc<dyn EffectHost> = Arc::new(ListingHost {
        inner: Arc::new(LayeredEffectHost::new(inner, mutation.clone())),
        listing,
        reads: AtomicUsize::new(0),
    });
    let (hold_send, hold_receive) = oneshot::channel();
    let make = || Arc::clone(&host);
    let law = lash_conformance::losing_wait_isolation_with_registration_witness(
        &make,
        &prefix,
        move |key| {
            if hold {
                hold_send
                    .send(hold_wait_registration(key))
                    .expect("the hold observer lives");
            }
            witness.then(|| {
                let registered = arm_wait_registration_witness(key);
                Box::pin(async move {
                    assert_eq!(
                        registered.await.expect("the backend answers registration"),
                        crate::RestateDurableWaitRegistration::Registered
                    );
                })
                    as std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>
            })
        },
    );
    let mut law = Box::pin(law);
    let law = std::future::poll_fn(move |cx| {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| law.as_mut().poll(cx))) {
            Ok(Poll::Ready(())) => Poll::Ready(Ok(())),
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic) => Poll::Ready(Err(panic)),
        }
    });
    tokio::pin!(law);
    let mut early_close = false;
    if hold {
        let (entered, release) = tokio::select! {
            hold = hold_receive => hold.expect("the law arms the companion hold"),
            result = &mut law => panic!("the law finished before arming its hold: {result:?}"),
        };
        tokio::select! {
            entered = entered => entered.expect("the companion reaches backend registration"),
            result = &mut law => panic!("the law finished before reaching registration: {result:?}"),
        }
        tokio::select! {
            _ = mutation.started.notified() => early_close = true,
            _ = tokio::time::sleep(Duration::from_millis(400)) => {},
            result = &mut law => panic!("the law finished while registration was held: {result:?}"),
        }
        if early_close && overbroad {
            tokio::select! {
                _ = mutation.swept.notified() => {},
                result = &mut law => panic!("the law finished before the session sweep: {result:?}"),
            }
        }
        release.send(()).expect("the companion is still held");
    }
    let result = tokio::time::timeout(Duration::from_secs(10), &mut law)
        .await
        .expect("the law must answer rather than swallow an error until its budget expires");
    harness.finish().await;
    if matches!(listing, Listing::InitialError | Listing::PollError) || overbroad || !witness {
        let panic = result.expect_err("the injected defect must fail the shared law");
        let message = panic
            .downcast_ref::<String>()
            .map(String::as_str)
            .or_else(|| panic.downcast_ref::<&str>().copied())
            .unwrap_or("non-string panic");
        let expected = if overbroad {
            "its other wait still resolves"
        } else if !witness {
            "registration witness"
        } else {
            "registry failure"
        };
        assert!(
            message.contains(expected),
            "the law failed for the intended reason: {message}"
        );
    } else {
        assert!(
            result.is_ok(),
            "the registered companion survives a scoped close"
        );
    }
    assert!(
        !early_close,
        "close started while companion registration was held"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_listing_error_fails_the_law() {
    run_mutation(Listing::InitialError, false, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subsequent_listing_error_fails_the_law() {
    run_mutation(Listing::PollError, false, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn held_companion_registration_prevents_close() {
    run_mutation(Listing::Unsupported, true, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overbroad_close_fails_the_law() {
    run_mutation(Listing::Unsupported, true, true, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_listing_uses_backend_registration_witness() {
    run_mutation(Listing::Unsupported, false, false, true).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_listing_requires_registration_witness() {
    run_mutation(Listing::Unsupported, false, false, false).await;
}
