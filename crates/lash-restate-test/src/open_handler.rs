//! The kernel door into a handler: one workflow handler execution on the
//! server double, lent to the caller's task.
//!
//! [`RestateTestBackend::run_in_handler`](crate::RestateTestBackend::run_in_handler)
//! moves a `'static` job into the handler. A kernel test instead holds its
//! runtime by `&mut` in its own task and hands the runtime a controller.
//! [`open_handler`](crate::RestateTestBackend::open_handler) starts a handler
//! that lends its Restate context to the caller and waits. The caller builds
//! the handler's scoped controller from that context with
//! [`OpenHandler::scoped`], runs the turn in its own task, and then
//! [`OpenHandler::close`]s the handler, which ends the invocation.
//!
//! An open handler serves one execution. The caller's code is not a handler
//! body the server can re-run, so a replay has nothing to replay into. If the
//! invocation suspends or replays (`always_replay`, a crash rule, an await the
//! server cannot answer in-stream), the lent context's next effect fails, and
//! `close` reports it. A test that exercises replay uses `run_in_handler`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lash_core::{AdmittedScope, ScopedEffectController};
use lash_restate::{RestateAuthorityId, RestateRuntimeEffectController};
use restate_sdk::endpoint::{ContextInternal, InputMetadata};
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::serde::Json;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::RestateTestBackend;

pub(crate) const HANDLER_LENDER: &str = "LashTestHandlerLender";

/// A handler execution lent to the caller: see the module docs.
///
/// Dropping it without [`close`](Self::close) releases the handler as a
/// failed attempt.
pub struct OpenHandler {
    context: LentContext,
    admitted: AdmittedScope,
    authority: RestateAuthorityId,
    release: oneshot::Sender<()>,
    invocation: JoinHandle<Result<bool, String>>,
}

impl std::fmt::Debug for OpenHandler {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OpenHandler")
            .field("admitted", &self.admitted)
            .finish_non_exhaustive()
    }
}

impl OpenHandler {
    /// The handler's controller for the scope it was opened for, the one a
    /// Restate deployment hands a turn. It borrows the handler, so it cannot
    /// outlive [`close`](Self::close).
    #[expect(
        clippy::expect_used,
        reason = "open_handler refused every scope this view could refuse before it lent the handler"
    )]
    pub fn scoped(&self) -> ScopedEffectController<'_> {
        let context = restate_sdk::context::WorkflowContext::from((
            &self.context.internal,
            self.context.metadata(),
        ));
        Arc::new(RestateRuntimeEffectController::new(
            context,
            self.authority.clone(),
        ))
        .into_scoped_effect_controller(self.admitted.clone())
        .expect("open_handler admitted this scope when it opened")
    }

    /// Ends the handler execution and returns once its invocation completed.
    pub async fn close(self) -> Result<(), String> {
        let Self {
            context,
            release,
            invocation,
            ..
        } = self;
        // The handler's context goes before the handler does: the invocation
        // ends only once nothing but the handler holds it.
        drop(context);
        release
            .send(())
            .map_err(|()| "the lent handler ended before it was closed".to_owned())?;
        match invocation.await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(error)) => Err(error),
            Err(join) => Err(format!("the lent handler's invocation task failed: {join}")),
        }
    }
}

impl RestateTestBackend {
    /// Open a workflow handler on the server under `admitted` and lend its
    /// execution to this task: see [`OpenHandler`].
    pub async fn open_handler(&self, admitted: AdmittedScope) -> Result<OpenHandler, String> {
        // The scope a lent controller may serve is the one `scoped` builds;
        // refuse a bad one here, not in the middle of the turn.
        admitted
            .scope()
            .validate()
            .map_err(|error| error.to_string())?;
        if matches!(admitted.scope(), lash_core::ExecutionScope::Process { .. }) {
            return Err(
                "a process segment's effects are admitted only by its committed start marker; \
                 open_handler serves session and turn scopes"
                    .to_owned(),
            );
        }
        let (lend, lent) = oneshot::channel();
        let (release, released) = oneshot::channel();
        let key = self.loans().park(Loan { lend, released });
        // A future dropped mid-await runs neither `select!` arm, so the loan
        // would sit parked for a re-invocation that never comes. The guard
        // takes it instead: the handler, when it eventually starts, finds the
        // key unparked and fails as the release semantics want.
        let _parked = LoanGuard {
            loans: self.loans(),
            key: key.clone(),
        };
        let ingress = self.ingress();
        let call_key = key.clone();
        let mut invocation = tokio::spawn(async move {
            ingress
                .call_workflow_json::<_, bool>(HANDLER_LENDER, &call_key, "run", &call_key)
                .await
                .map_err(|error| format!("the lent handler did not complete: {error}"))
        });
        let context = tokio::select! {
            context = lent => context,
            ended = &mut invocation => {
                return Err(match ended {
                    Ok(Ok(_)) => "the lent handler ended before it lent its context".to_owned(),
                    Ok(Err(error)) => error,
                    Err(join) => format!("the lent handler's invocation task failed: {join}"),
                });
            }
        };
        let context =
            context.map_err(|_| "the lent handler ended before it lent its context".to_owned())?;
        Ok(OpenHandler {
            context,
            admitted,
            authority: self.authority().clone(),
            release,
            invocation,
        })
    }
}

/// A handler's Restate context, owned: the SDK context shares one invocation
/// state, so the clone reaches the same journal as the handler's own.
pub(crate) struct LentContext {
    internal: ContextInternal,
    invocation_id: String,
    random_seed: u64,
    key: String,
    headers: http::HeaderMap<String>,
    scope: Option<String>,
    limit_key: Option<String>,
    idempotency_key: Option<String>,
}

impl LentContext {
    fn metadata(&self) -> InputMetadata {
        InputMetadata {
            invocation_id: self.invocation_id.clone(),
            random_seed: self.random_seed,
            key: self.key.clone(),
            headers: self.headers.clone(),
            scope: self.scope.clone(),
            limit_key: self.limit_key.clone(),
            idempotency_key: self.idempotency_key.clone(),
        }
    }
}

/// A parked loan's cleanup on the opening side: when `open_handler`'s
/// select ends in any way — context lent, invocation ended, or the whole
/// future dropped mid-await — the loan leaves the map.
struct LoanGuard<'a> {
    loans: &'a Loans,
    key: String,
}

impl Drop for LoanGuard<'_> {
    fn drop(&mut self) {
        self.loans.take(&self.key);
    }
}

/// One open handler waiting to start: where it lends its context, and the
/// release it waits for.
pub(crate) struct Loan {
    lend: oneshot::Sender<LentContext>,
    released: oneshot::Receiver<()>,
}

#[derive(Default)]
pub(crate) struct Loans {
    next: AtomicU64,
    loans: Mutex<HashMap<String, Loan>>,
}

impl Loans {
    fn park(&self, loan: Loan) -> String {
        let key = format!("loan-{}", self.next.fetch_add(1, Ordering::SeqCst));
        self.loans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.clone(), loan);
        key
    }

    fn take(&self, key: &str) -> Option<Loan> {
        self.loans
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(key)
    }
}

/// The workflow an open handler runs in: one key per loan.
pub(crate) struct HandlerLender {
    pub(crate) loans: Arc<Loans>,
}

/// The handler's context, taken whole.
///
/// The SDK's macro names the context type by its last path segment, so this
/// type carries the SDK's name. It captures what the SDK's `WorkflowContext`
/// borrows: the invocation's shared state and its metadata.
mod lent {
    pub(crate) struct WorkflowContext(pub(crate) super::LentContext);

    impl From<(&super::ContextInternal, super::InputMetadata)> for WorkflowContext {
        fn from((internal, metadata): (&super::ContextInternal, super::InputMetadata)) -> Self {
            Self(super::LentContext {
                internal: internal.clone(),
                invocation_id: metadata.invocation_id,
                random_seed: metadata.random_seed,
                key: metadata.key,
                headers: metadata.headers,
                scope: metadata.scope,
                limit_key: metadata.limit_key,
                idempotency_key: metadata.idempotency_key,
            })
        }
    }
}

#[restate_sdk::workflow(name = "LashTestHandlerLender")]
impl HandlerLender {
    #[handler]
    async fn run(
        &self,
        context: lent::WorkflowContext,
        Json(key): Json<String>,
    ) -> HandlerResult<Json<bool>> {
        let Some(Loan { lend, released }) = self.loans.take(&key) else {
            return Err(TerminalError::new(format!(
                "loan `{key}` is not parked on this backend; an open handler serves one \
                 execution, and its invocation was re-run"
            ))
            .into());
        };
        if lend.send(context.0).is_err() {
            return Err(TerminalError::new(format!(
                "loan `{key}` was abandoned before its handler started"
            ))
            .into());
        }
        match released.await {
            Ok(()) => Ok(Json(true)),
            Err(_) => Err(TerminalError::new(format!(
                "loan `{key}` was dropped without being closed"
            ))
            .into()),
        }
    }
}
