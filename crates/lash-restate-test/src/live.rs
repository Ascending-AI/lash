//! The same lash backend against a live `restate-server`.
//!
//! [`LiveRestateBackend`] is [`RestateTestBackend`](crate::RestateTestBackend)'s
//! counterpart for a real server: lash-restate's engine over a SQLite memory
//! store set, with every lash-restate service (and the handler host
//! [`run_in_handler`](LiveRestateBackend::run_in_handler) enters) bound on one
//! endpoint that this process serves over HTTP/2 on loopback and registers
//! with the server's admin API. Nothing in between is simulated: the server's
//! invoker, journals, retries, timers and admin operations are the pinned
//! `restate-server`'s own.
//!
//! What the backend adds is the deployment's own faults, the ones a real
//! deployment has:
//!
//! - **The deployment dies.** [`stop_serving`](LiveRestateBackend::stop_serving)
//!   closes the endpoint's listener and drops every connection and every
//!   stream it served, so each attempt the server was running on it fails
//!   with a reset connection, and connections to it are refused until
//!   [`start_serving`](LiveRestateBackend::start_serving) listens again on the
//!   same address. The server retries each such attempt on its retry policy
//!   and replays its journal into the deployment that comes back.
//! - **The deployment dies at a journal step.** A [`CrashRule`] armed with
//!   [`crash_on`](LiveRestateBackend::crash_on) is matched against every
//!   protocol frame an attempt sends, exactly as the server double matches
//!   it; the frame that matches never leaves the deployment: the deployment
//!   dies right there, before the server could store it.
//! - **The deployment refuses an object's work.** A [`hold`](LiveRestateBackend::hold)
//!   on a service, or on one of its keys, answers every new attempt of a
//!   held invocation `503` (the server backs off and retries it) and resets
//!   a running one, until the hold is released: the double's hold, made of
//!   the deployment's own answers.
//!
//! The backend's store clock is the wall clock plus an offset
//! [`advance`](LiveRestateBackend::advance) moves, so a scenario can let
//! store time (leases, claims, due times) pass without waiting for it, while
//! the server's own timers and retries run on wall time.

use std::collections::HashMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use bytes::{Bytes, BytesMut};
use http_body::{Body, Frame as BodyFrame};
use http_body_util::BodyExt as _;
use lash_core::{AdmittedScope, SessionWorkEngine, StoreSet};
use lash_core_worker::DurableProcessWorker;
use lash_restate::{
    RestateAdminClient, RestateAuthorityId, RestateConfig, RestateConnection, RestateEngine,
    RestateIngressClient, RestateInvocationId, RestateNamespace, RestateProcessServing,
    RestateProcessWorkerSlot, RestateRegistrationError,
};
use restate_sdk::endpoint::{Endpoint, HandleOptions, ProtocolMode};

use crate::backend::{
    ExplicitlyReconciledSessionWork, HANDLER_HOST, HandlerAttempt, HandlerHost, Parked, ParkedJobs,
    bind_handler_host,
};
use crate::protocol::generated::{ProposeRunCompletionMessage, RunCommandMessage, StartMessage};
use crate::protocol::{FrameDecoder, MessageType};
use crate::server::{CrashListener, CrashPlan, CrashRule, CrashSite};

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Where the live server and this backend's endpoint are.
#[derive(Clone, Debug)]
pub struct LiveConfig {
    /// The server's ingress API.
    pub ingress_url: String,
    /// The server's admin API.
    pub admin_url: String,
    /// The loopback address the backend's endpoint listens on.
    pub endpoint_bind: SocketAddr,
    /// The URL the server reaches the endpoint at.
    pub endpoint_url: String,
    /// A tag unique to this backend on the server: a workflow key runs once
    /// per server, and the server outlives one backend.
    pub run_tag: String,
    /// The namespace the backend's services are named in (FIG-3898): two
    /// backends serve one server side by side in distinct namespaces.
    pub namespace: RestateNamespace,
}

/// Why a live backend could not come up.
#[derive(Debug, thiserror::Error)]
pub enum LiveError {
    #[error("the SQLite memory store set could not open: {0}")]
    Stores(String),
    #[error("the Restate authority id is invalid: {0}")]
    Authority(String),
    #[error("the endpoint could not listen on {bind}: {detail}")]
    Listen { bind: SocketAddr, detail: String },
    #[error("the deployment at {url} could not register: {detail}")]
    Register { url: String, detail: String },
    /// The engine refused the registration: another deployment on the
    /// server serves its names (FIG-3898).
    #[error(transparent)]
    Registration(Box<RestateRegistrationError>),
    #[error("the handler host's name is not a Restate service name: {0}")]
    HandlerHostName(String),
    #[error("the admin API failed: {0}")]
    Admin(String),
}

impl From<RestateRegistrationError> for LiveError {
    fn from(error: RestateRegistrationError) -> Self {
        Self::Registration(Box::new(error))
    }
}

// ---------------------------------------------------------------------------
// The clock
// ---------------------------------------------------------------------------

/// The store clock of a live backend: wall time plus an offset only
/// [`advance`](Self::advance) moves.
#[derive(Debug, Default)]
pub struct LiveClock {
    offset_ms: AtomicU64,
}

impl LiveClock {
    /// Move the clock `by` ahead of wall time, on top of what it was.
    pub fn advance(&self, by: Duration) {
        self.offset_ms.fetch_add(
            u64::try_from(by.as_millis()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
    }

    /// The epoch milliseconds the clock reads now.
    pub fn now_ms(&self) -> u64 {
        use lash_core::ClockWallTime as _;
        self.timestamp_ms()
    }
}

#[async_trait::async_trait]
impl lash_core::Clock for LiveClock {
    fn now(&self) -> Instant {
        Instant::now()
    }

    fn timestamp_datetime(&self) -> chrono::DateTime<chrono::Utc> {
        let offset = i64::try_from(self.offset_ms.load(Ordering::SeqCst)).unwrap_or(i64::MAX);
        chrono::Utc::now() + chrono::Duration::milliseconds(offset)
    }

    async fn sleep(&self, duration: Duration) {
        tokio::time::sleep(duration).await;
    }

    async fn sleep_until(&self, deadline: Instant) {
        tokio::time::sleep_until(deadline.into()).await;
    }
}

// ---------------------------------------------------------------------------
// Admin views
// ---------------------------------------------------------------------------

/// One row of the server's `sys_invocation` table, as the backend reads it.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct LiveInvocation {
    pub id: String,
    /// `Service/key/handler`, or `Service/handler` without a key.
    pub target: String,
    /// The server's lifecycle name: `pending`, `scheduled`, `ready`,
    /// `running`, `suspended`, `backing-off`, `paused` or `completed`.
    pub status: String,
    #[serde(default)]
    pub retry_count: Option<u64>,
    #[serde(default)]
    pub journal_size: Option<u64>,
    #[serde(default)]
    pub last_failure: Option<String>,
    #[serde(default)]
    pub completion_result: Option<String>,
    #[serde(default)]
    pub completion_failure: Option<String>,
}

/// What [`LiveRestateBackend::settle`] compares between reads: an open
/// invocation's id, status, retry count and journal length.
type SettleRow = (String, String, Option<u64>, Option<u64>);

const INVOCATION_COLUMNS: &str = "id, target, status, retry_count, journal_size, last_failure, completion_result, completion_failure";

/// An invocation status that moves on by itself: work the server is about to
/// hand the deployment.
fn in_motion(status: &str) -> bool {
    matches!(status, "pending" | "scheduled" | "ready" | "backing-off")
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

// ---------------------------------------------------------------------------
// The backend
// ---------------------------------------------------------------------------

/// See the module documentation.
#[derive(Clone)]
pub struct LiveRestateBackend {
    inner: Arc<Inner>,
}

struct Inner {
    config: LiveConfig,
    restate: Arc<RestateEngine>,
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    clock: Arc<LiveClock>,
    connection: RestateConnection,
    admin: RestateAdminClient,
    processes: RestateProcessWorkerSlot,
    jobs: Arc<ParkedJobs>,
    serving: Arc<Serving>,
}

/// The last handle gone, the deployment dies: its accept loop holds the
/// endpoint's address, which the next backend binds.
impl Drop for Inner {
    fn drop(&mut self) {
        self.serving.stop(true);
    }
}

impl std::fmt::Debug for LiveRestateBackend {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LiveRestateBackend")
            .field("endpoint", &self.inner.config.endpoint_url)
            .field("run_tag", &self.inner.config.run_tag)
            .finish_non_exhaustive()
    }
}

/// The URI a backend registers its endpoint at: the endpoint URL itself in
/// the default namespace, `<endpoint_url>/ns/<namespace>` otherwise. The
/// endpoint serves any path prefix.
fn deployment_url(config: &LiveConfig) -> String {
    let base = config.endpoint_url.trim_end_matches('/');
    if config.namespace.is_default() {
        base.to_owned()
    } else {
        format!("{base}/ns/{}", config.namespace.as_str())
    }
}

impl LiveRestateBackend {
    /// Build the engine over a fresh SQLite memory store set, serve its
    /// endpoint on `config.endpoint_bind` and register it with the server.
    pub async fn start(config: LiveConfig) -> Result<Self, LiveError> {
        Self::start_on(config, None).await
    }

    /// [`start`](Self::start), with an endpoint that cuts every process
    /// segment after `segment_effect_budget` completed effects instead of
    /// the default 10,000, so a short process crosses segment boundaries:
    /// the live counterpart of
    /// [`backend_with_segment_budget`](crate::backend_with_segment_budget).
    pub async fn start_with_segment_budget(
        config: LiveConfig,
        segment_effect_budget: u64,
    ) -> Result<Self, LiveError> {
        Self::start_on(config, Some(segment_effect_budget)).await
    }

    async fn start_on(
        config: LiveConfig,
        segment_effect_budget: Option<u64>,
    ) -> Result<Self, LiveError> {
        let clock = Arc::new(LiveClock::default());
        // Process ids are minted at random: the server keys a process's
        // workflow by its id, and it outlives this backend.
        let stores = Arc::new(
            lash_sqlite_store::SqliteStoreSet::memory_with_options_and_clock(
                lash_sqlite_store::SqliteStoreSetOptions::memory(),
                Arc::clone(&clock) as Arc<dyn lash_core::Clock>,
            )
            .await
            .map_err(|error| LiveError::Stores(error.to_string()))?,
        );
        let connection = RestateConnection::new(config.ingress_url.clone());
        let admin_connection = RestateConnection::new(config.admin_url.clone());
        let authority = RestateAuthorityId::new(format!("lash-live-{}", config.run_tag))
            .map_err(|error| LiveError::Authority(error.to_string()))?;
        let restate = Arc::new(RestateEngine::new(
            Arc::clone(&stores) as Arc<dyn StoreSet>,
            RestateConfig::new(
                connection.clone(),
                admin_connection.clone(),
                authority.clone(),
                lash_core::engine::BuildGeneration::for_test("t0"),
            )
            .with_namespace(config.namespace.clone()),
        ));
        let processes = RestateProcessWorkerSlot::new();
        let jobs = Arc::new(ParkedJobs::with_prefix(format!("{}-", config.run_tag)));
        let serving = RestateProcessServing::from(processes.clone());
        let serving = match segment_effect_budget {
            Some(budget) => serving.with_segment_effect_budget_selector(move |_| budget),
            None => serving,
        };
        let endpoint = bind_handler_host(
            restate.endpoint_builder(serving),
            HandlerHost {
                jobs: Arc::clone(&jobs),
                authority,
                namespace: config.namespace.clone(),
            },
        )
        .map_err(LiveError::HandlerHostName)?
        .build();
        let serving = Arc::new(Serving {
            endpoint,
            bind: config.endpoint_bind,
            plan: Mutex::new(CrashPlan::default()),
            attempts: Mutex::new(HashMap::new()),
            listener: OnceLock::new(),
            live: Mutex::new(None),
            jobs: Arc::clone(&jobs),
            holds: Mutex::new(Vec::new()),
            next_hold: AtomicU64::new(0),
            running: Mutex::new(Vec::new()),
        });
        let backend = Self {
            inner: Arc::new(Inner {
                config,
                restate,
                stores,
                clock,
                connection,
                admin: RestateAdminClient::new(admin_connection),
                processes,
                jobs,
                serving,
            }),
        };
        backend.start_serving().await?;
        backend.register().await?;
        Ok(backend)
    }

    /// Register the endpoint through the engine, which refuses a namespace
    /// another deployment on the server serves (FIG-3898). A deployment
    /// registered earlier at this backend's own URL — a world before this
    /// one at the same address and in the same namespace — is replaced.
    ///
    /// A namespaced backend registers under a path of its namespace
    /// ([`deployment_url`]): a deployment in another namespace is another
    /// deployment, and the engine refuses a URI another deployment holds
    /// (ADR 0115 §3.5), so two namespaces served in turn at one address
    /// never share a URI.
    async fn register(&self) -> Result<(), LiveError> {
        let url = deployment_url(&self.inner.config);
        self.inner
            .restate
            .register_deployment(&url)
            .await
            .map_err(|error| match error {
                RestateRegistrationError::Admin(error) => LiveError::Register {
                    url,
                    detail: error.to_string(),
                },
                refusal => LiveError::Registration(Box::new(refusal)),
            })
    }

    /// The namespace this backend's services are named in.
    pub fn namespace(&self) -> &RestateNamespace {
        &self.inner.config.namespace
    }

    /// `name`'s Restate name in this backend's namespace.
    pub fn service_name(&self, name: &str) -> String {
        self.inner.config.namespace.service_name(name)
    }

    /// The backend a runtime runs on: lash-restate's engine over the store
    /// set, connected to the live server.
    pub fn lash_backend(&self) -> lash_core::Backend {
        lash_core::Backend::new(self.inner.restate.clone())
    }

    /// The engine's session work without its wall-clock reconcile interval;
    /// see [`RestateTestBackend::explicit_reconcile_session_work`](crate::RestateTestBackend::explicit_reconcile_session_work).
    pub fn explicit_reconcile_session_work(&self) -> Arc<dyn SessionWorkEngine> {
        Arc::new(ExplicitlyReconciledSessionWork {
            inner: Arc::clone(self.inner.restate.session_work_engine()),
        })
    }

    /// The store set the engine runs over.
    pub fn stores(&self) -> &Arc<lash_sqlite_store::SqliteStoreSet> {
        &self.inner.stores
    }

    /// The store clock.
    pub fn clock(&self) -> Arc<LiveClock> {
        Arc::clone(&self.inner.clock)
    }

    /// Store time now, in epoch milliseconds.
    pub fn now_ms(&self) -> u64 {
        self.inner.clock.now_ms()
    }

    /// Let `by` of store time pass at once.
    pub fn advance(&self, by: Duration) {
        self.inner.clock.advance(by);
    }

    /// The ingress client over the live server.
    pub fn ingress(&self) -> RestateIngressClient {
        RestateIngressClient::new(self.inner.connection.clone())
    }

    /// Serve process segments with `worker`.
    pub fn install_process_worker(&self, worker: DurableProcessWorker) {
        self.inner.processes.install(worker);
    }

    /// Run `attempt` inside a workflow handler on the live server, on the
    /// handler's scoped controller for `admitted`; see
    /// [`RestateTestBackend::run_in_handler`](crate::RestateTestBackend::run_in_handler).
    pub async fn run_in_handler(
        &self,
        admitted: AdmittedScope,
        attempt: HandlerAttempt,
    ) -> Result<(), String> {
        let key = self.inner.jobs.park(admitted, Parked::Replayed(attempt));
        let ran = self
            .ingress()
            .call_workflow_json::<_, bool>(&self.service_name(HANDLER_HOST), &key, "run", &key)
            .await;
        self.inner.jobs.take(&key);
        match ran {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!("job `{key}` reported failure")),
            Err(error) => Err(format!(
                "job `{key}` did not complete in its handler: {error}"
            )),
        }
    }

    // --- The deployment's faults -------------------------------------------

    /// Arm a journal-step crash: the frame it matches never leaves the
    /// deployment, which dies there.
    pub fn crash_on(&self, rule: CrashRule) {
        self.inner
            .serving
            .plan
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .add(rule);
    }

    /// Call `listener` with the crashed invocation's target every time an
    /// armed crash kills the deployment (once; a second listener is
    /// refused).
    pub fn on_crash(&self, listener: CrashListener) -> bool {
        self.inner.serving.listener.set(listener).is_ok()
    }

    /// Hold `service`'s invocations — every one, or those on `key` — until
    /// the returned [`LiveHold`] is released or dropped: the deployment
    /// answers each new attempt of one `503`, and resets one running now.
    pub fn hold(&self, service: &str, key: Option<&str>) -> LiveHold {
        let target = HoldTarget {
            id: self.inner.serving.next_hold.fetch_add(1, Ordering::SeqCst),
            service: service.to_owned(),
            key: key.map(str::to_owned),
        };
        let id = target.id;
        self.inner
            .serving
            .holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(target);
        self.inner.serving.reset_held();
        LiveHold {
            serving: Arc::downgrade(&self.inner.serving),
            id: Some(id),
        }
    }

    /// Take the deployment off the network: its listener closes, every
    /// connection and stream it served is dropped, and every attempt the
    /// server ran on it fails. With `host_died`, the host's own handler jobs
    /// die with it and a replay of one finds nothing to run.
    pub fn stop_serving(&self, host_died: bool) {
        self.inner.serving.stop(host_died);
    }

    /// Listen again on the endpoint's address.
    pub async fn start_serving(&self) -> Result<(), LiveError> {
        Arc::clone(&self.inner.serving).start().await
    }

    // --- The admin face ----------------------------------------------------

    /// Every invocation of this backend's namespace the server holds,
    /// completed ones included.
    pub async fn invocations(&self) -> Result<Vec<LiveInvocation>, LiveError> {
        self.query(&format!(
            "SELECT {INVOCATION_COLUMNS} FROM sys_invocation WHERE {} ORDER BY created_at",
            self.own_invocations()
        ))
        .await
    }

    /// The `sys_invocation` filter on this backend's namespace: another
    /// backend's invocations on the same server are never this one's to
    /// read, settle or kill. A namespace holds no `.` and no `_`, so neither
    /// pattern reaches past its own names.
    fn own_invocations(&self) -> String {
        let namespace = &self.inner.config.namespace;
        if namespace.is_default() {
            "target_service_name NOT LIKE '%.%'".to_owned()
        } else {
            format!(
                "target_service_name LIKE {}",
                sql_literal(&format!("{namespace}.%"))
            )
        }
    }

    /// One invocation's row.
    pub async fn invocation(&self, id: &str) -> Result<Option<LiveInvocation>, LiveError> {
        Ok(self
            .query(&format!(
                "SELECT {INVOCATION_COLUMNS} FROM sys_invocation WHERE id = {}",
                sql_literal(id)
            ))
            .await?
            .pop())
    }

    /// How `id` completed: `Ok` on success, `Err` with its failure; `None`
    /// while it has not.
    pub async fn outcome(&self, id: &str) -> Result<Option<Result<(), String>>, LiveError> {
        Ok(self
            .invocation(id)
            .await?
            .filter(|row| row.status == "completed")
            .map(|row| match row.completion_result.as_deref() {
                Some("success") => Ok(()),
                _ => Err(row
                    .completion_failure
                    .unwrap_or_else(|| "failed without a recorded failure".to_owned())),
            }))
    }

    /// The journal entries of `id`, named: `index:type:name`.
    pub async fn journal(&self, id: &str) -> Result<Vec<String>, LiveError> {
        #[derive(serde::Deserialize)]
        struct Entry {
            index: u64,
            entry_type: String,
            #[serde(default)]
            name: Option<String>,
        }
        let entries: Vec<Entry> = self
            .inner
            .admin
            .query_json(&format!(
                "SELECT index, entry_type, name FROM sys_journal WHERE id = {} ORDER BY index",
                sql_literal(id)
            ))
            .await
            .map_err(|error| LiveError::Admin(error.to_string()))?;
        Ok(entries
            .into_iter()
            .map(|entry| {
                format!(
                    "{}:{}:{}",
                    entry.index,
                    entry.entry_type,
                    entry.name.unwrap_or_default()
                )
            })
            .collect())
    }

    /// Kill `id` as an operator does and wait until the server completed it.
    /// Answers whether it was still open.
    pub async fn kill_and_await(&self, id: &str) -> Result<bool, LiveError> {
        match self.invocation(id).await? {
            Some(row) if row.status != "completed" => {}
            _ => return Ok(false),
        }
        self.inner
            .admin
            .kill_invocation(&RestateInvocationId::new(id))
            .await
            .map_err(|error| LiveError::Admin(format!("kill `{id}`: {error}")))?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            match self.invocation(id).await? {
                Some(row) if row.status != "completed" => {}
                _ => return Ok(true),
            }
            if tokio::time::Instant::now() > deadline {
                return Err(LiveError::Admin(format!(
                    "`{id}` was killed and never completed"
                )));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// Wait until the server has nothing moving: two reads of every open
    /// invocation, `interval` apart, agree, and none is about to be handed
    /// to the deployment. Bounded by `budget` in wall time.
    pub async fn settle(&self, budget: Duration, interval: Duration) {
        let deadline = tokio::time::Instant::now() + budget;
        let mut last: Option<Vec<SettleRow>> = None;
        while tokio::time::Instant::now() < deadline {
            let snapshot = match self
                .query(&format!(
                    "SELECT {INVOCATION_COLUMNS} FROM sys_invocation WHERE status != 'completed' AND {} ORDER BY id",
                    self.own_invocations()
                ))
                .await
            {
                Ok(rows) => rows,
                Err(_) => {
                    tokio::time::sleep(interval).await;
                    continue;
                }
            };
            let moving = snapshot.iter().any(|row| in_motion(&row.status));
            let snapshot: Vec<_> = snapshot
                .into_iter()
                .map(|row| (row.id, row.status, row.retry_count, row.journal_size))
                .collect();
            if !moving && last.as_ref() == Some(&snapshot) {
                return;
            }
            last = Some(snapshot);
            tokio::time::sleep(interval).await;
        }
    }

    /// End the backend: the deployment dies, and every invocation the server
    /// still holds open is killed, so nothing of it reaches the endpoint the
    /// next backend serves at the same address.
    pub async fn finish(&self) {
        self.stop_serving(true);
        for _ in 0..3 {
            let open = match self
                .query(&format!(
                    "SELECT {INVOCATION_COLUMNS} FROM sys_invocation WHERE status != 'completed' AND {}",
                    self.own_invocations()
                ))
                .await
            {
                Ok(open) => open,
                Err(_) => return,
            };
            if open.is_empty() {
                return;
            }
            for row in open {
                let _ = self.kill_and_await(&row.id).await;
            }
        }
    }

    async fn query(&self, sql: &str) -> Result<Vec<LiveInvocation>, LiveError> {
        self.inner
            .admin
            .query_json(sql)
            .await
            .map_err(|error| LiveError::Admin(error.to_string()))
    }
}

// ---------------------------------------------------------------------------
// Serving the endpoint
// ---------------------------------------------------------------------------

/// The endpoint's HTTP/2 serving, with the crash plan every attempt's frames
/// are matched against.
struct Serving {
    endpoint: Endpoint,
    bind: SocketAddr,
    plan: Mutex<CrashPlan>,
    /// Attempts started per invocation id, counted from the start frames the
    /// server sent.
    attempts: Mutex<HashMap<Bytes, u32>>,
    listener: OnceLock<CrashListener>,
    live: Mutex<Option<Live>>,
    jobs: Arc<ParkedJobs>,
    holds: Mutex<Vec<HoldTarget>>,
    next_hold: AtomicU64,
    /// Every attempt the deployment serves, for a hold to reset.
    running: Mutex<Vec<Weak<Mutex<AttemptState>>>>,
}

/// A held service, or one key of it.
#[derive(Clone, Debug)]
struct HoldTarget {
    id: u64,
    service: String,
    key: Option<String>,
}

impl HoldTarget {
    fn covers(&self, service: &str, key: Option<&str>) -> bool {
        self.service == service && (self.key.is_none() || self.key.as_deref() == key)
    }
}

/// A hold taken with [`LiveRestateBackend::hold`]; dropping it releases it.
#[derive(Debug)]
#[must_use = "dropping a hold releases it"]
pub struct LiveHold {
    serving: Weak<Serving>,
    id: Option<u64>,
}

impl LiveHold {
    /// Release the hold now: the server's next retry of each held attempt
    /// runs.
    pub fn release(mut self) {
        self.release_now();
    }

    fn release_now(&mut self) {
        let (Some(serving), Some(id)) = (self.serving.upgrade(), self.id.take()) else {
            return;
        };
        serving
            .holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|hold| hold.id != id);
    }
}

impl Drop for LiveHold {
    fn drop(&mut self) {
        self.release_now();
    }
}

/// One period of the deployment being up: its accept loop, and every task
/// the connections it accepted spawned.
struct Live {
    accept: tokio::task::AbortHandle,
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl Serving {
    fn stop(&self, host_died: bool) {
        let live = self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(live) = live {
            live.accept.abort();
            for task in live
                .tasks
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .drain(..)
            {
                task.abort();
            }
        }
        if host_died {
            self.jobs.clear();
        }
    }

    async fn start(self: Arc<Self>) -> Result<(), LiveError> {
        if self
            .live
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
        {
            return Ok(());
        }
        // The listener a stop aborted closes when its task next runs; until
        // then the address is still taken.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let listener = loop {
            match tokio::net::TcpListener::bind(self.bind).await {
                Ok(listener) => break listener,
                Err(error) if tokio::time::Instant::now() < deadline => {
                    let _ = error;
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(error) => {
                    return Err(LiveError::Listen {
                        bind: self.bind,
                        detail: error.to_string(),
                    });
                }
            }
        };
        let tasks = Arc::new(Mutex::new(Vec::new()));
        let executor = TrackedExecutor {
            tasks: Arc::clone(&tasks),
        };
        let serving = Arc::clone(&self);
        let accept = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    // A refused accept (the process's descriptors spent, say)
                    // is retried, not spun on.
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    continue;
                };
                let service = CutService {
                    serving: Arc::clone(&serving),
                };
                let connection = hyper::server::conn::http2::Builder::new(executor.clone())
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service);
                hyper::rt::Executor::execute(&executor, async move {
                    let _ = connection.await;
                });
            }
        });
        *self.live.lock().unwrap_or_else(PoisonError::into_inner) = Some(Live {
            accept: accept.abort_handle(),
            tasks,
        });
        Ok(())
    }

    fn holds_service(&self, service: &str) -> bool {
        self.holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|hold| hold.service == service)
    }

    fn is_held(&self, service: &str, key: Option<&str>) -> bool {
        self.holds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .any(|hold| hold.covers(service, key))
    }

    /// Reset every running attempt a hold now covers.
    fn reset_held(&self) {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        running.retain(|attempt| attempt.strong_count() > 0);
        for attempt in running.iter().filter_map(Weak::upgrade) {
            let mut state = attempt.lock().unwrap_or_else(PoisonError::into_inner);
            if self.is_held(&state.service, state.key.as_deref()) {
                state.held = true;
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
            }
        }
    }

    /// The frame an attempt sends crashes the deployment: tell the listener
    /// and take the deployment down.
    fn crash(&self, target: &str) {
        if let Some(listener) = self.listener.get() {
            listener(target);
        }
        self.stop(true);
    }
}

/// Spawns every task a connection needs on Tokio and keeps its abort handle,
/// so a dying deployment drops its streams, not only its connections.
#[derive(Clone)]
struct TrackedExecutor {
    tasks: Arc<Mutex<Vec<tokio::task::AbortHandle>>>,
}

impl<F> hyper::rt::Executor<F> for TrackedExecutor
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    fn execute(&self, future: F) {
        let handle = tokio::spawn(future);
        let mut tasks = self.tasks.lock().unwrap_or_else(PoisonError::into_inner);
        tasks.retain(|task| !task.is_finished());
        tasks.push(handle.abort_handle());
    }
}

/// One attempt's view of its protocol stream, fed by both directions.
struct AttemptState {
    service: String,
    handler: String,
    key: Option<String>,
    attempt: u32,
    /// Journal entries the server replays after the start frame.
    known_entries: Option<u32>,
    replayed: u32,
    /// Journal commands so far: replayed, then sent.
    commands: usize,
    /// A run's result completion id: its name and journal command index.
    runs: HashMap<u32, (String, usize)>,
    request: FrameDecoder,
    response: FrameDecoder,
    response_raw: BytesMut,
    cut: bool,
    /// A hold reset this attempt.
    held: bool,
    /// The waker of the stream serving the attempt, for a hold to reset it.
    waker: Option<Waker>,
}

impl AttemptState {
    fn target(&self) -> String {
        match &self.key {
            Some(key) => format!("{}/{}/{}", self.service, key, self.handler),
            None => format!("{}/{}", self.service, self.handler),
        }
    }

    fn record_command(&mut self, ty: MessageType, payload: &Bytes) {
        if ty == MessageType::RunCommand
            && let Ok(run) = <RunCommandMessage as prost::Message>::decode(payload.clone())
        {
            self.runs
                .insert(run.result_completion_id, (run.name, self.commands));
        }
        self.commands += 1;
    }
}

#[derive(Clone)]
struct CutService {
    serving: Arc<Serving>,
}

type Answer = http::Response<CutBody>;

impl CutService {
    /// Serve one attempt of `service`'s `handler` over `body`, which may
    /// start with bytes already read from the request.
    fn serve(
        &self,
        parts: http::request::Parts,
        body: Prefixed,
        service: String,
        handler: String,
    ) -> Answer {
        let attempt = Arc::new(Mutex::new(AttemptState {
            service,
            handler,
            key: None,
            attempt: 0,
            known_entries: None,
            replayed: 0,
            commands: 0,
            runs: HashMap::new(),
            request: FrameDecoder::default(),
            response: FrameDecoder::default(),
            response_raw: BytesMut::new(),
            cut: false,
            held: false,
            waker: None,
        }));
        {
            let mut running = self
                .serving
                .running
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            running.retain(|attempt| attempt.strong_count() > 0);
            running.push(Arc::downgrade(&attempt));
        }
        let tapped = TapBody {
            inner: body,
            attempt: Arc::clone(&attempt),
            serving: Arc::clone(&self.serving),
        };
        let response = self.serving.endpoint.handle_with_options(
            http::Request::from_parts(parts, tapped),
            HandleOptions {
                protocol_mode: ProtocolMode::BidiStream,
            },
        );
        response.map(|body| CutBody {
            inner: Some(body),
            attempt: Some(attempt),
            serving: Arc::clone(&self.serving),
            queued: std::collections::VecDeque::new(),
        })
    }

    fn plain(&self, response: http::Response<restate_sdk::endpoint::ResponseBody>) -> Answer {
        response.map(|body| CutBody {
            inner: Some(body),
            attempt: None,
            serving: Arc::clone(&self.serving),
            queued: std::collections::VecDeque::new(),
        })
    }

    /// The answer to an attempt of a held invocation: unavailable, so the
    /// server backs off and retries it.
    fn refused(&self) -> Answer {
        let mut response = http::Response::new(CutBody {
            inner: None,
            attempt: None,
            serving: Arc::clone(&self.serving),
            queued: std::collections::VecDeque::new(),
        });
        *response.status_mut() = http::StatusCode::SERVICE_UNAVAILABLE;
        response
    }
}

impl hyper::service::Service<http::Request<hyper::body::Incoming>> for CutService {
    type Response = Answer;
    type Error = std::convert::Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Answer, Self::Error>> + Send>>;

    fn call(&self, request: http::Request<hyper::body::Incoming>) -> Self::Future {
        let segments: Vec<&str> = request.uri().path().split('/').collect();
        let invoked = match segments.get(segments.len().saturating_sub(3)..) {
            Some([invoke, service, handler]) if *invoke == "invoke" => {
                Some(((*service).to_owned(), (*handler).to_owned()))
            }
            _ => None,
        };
        let Some((service, handler)) = invoked else {
            let response = self.serving.endpoint.handle_with_options(
                request,
                HandleOptions {
                    protocol_mode: ProtocolMode::BidiStream,
                },
            );
            return Box::pin(std::future::ready(Ok(self.plain(response))));
        };
        let (parts, body) = request.into_parts();
        if !self.serving.holds_service(&service) {
            let answer = self.serve(
                parts,
                Prefixed {
                    prefix: None,
                    inner: body,
                },
                service,
                handler,
            );
            return Box::pin(std::future::ready(Ok(answer)));
        }
        // A hold covers the service: read the start frame for the key.
        let this = self.clone();
        Box::pin(async move {
            let mut body = body;
            let mut read = BytesMut::new();
            let mut decoder = FrameDecoder::default();
            let key = loop {
                match body.frame().await {
                    Some(Ok(frame)) => {
                        let Some(data) = frame.data_ref() else {
                            continue;
                        };
                        read.extend_from_slice(data);
                        decoder.push(data);
                        match decoder.next_frame() {
                            Ok(None) => {}
                            Ok(Some(frame)) if frame.ty == MessageType::Start => {
                                break frame
                                    .decode::<StartMessage>()
                                    .ok()
                                    .map(|start| start.key)
                                    .filter(|key| !key.is_empty());
                            }
                            Ok(Some(_)) | Err(_) => break None,
                        }
                    }
                    Some(Err(_)) | None => break None,
                }
            };
            if this.serving.is_held(&service, key.as_deref()) {
                return Ok(this.refused());
            }
            Ok(this.serve(
                parts,
                Prefixed {
                    prefix: Some(read.freeze()),
                    inner: body,
                },
                service,
                handler,
            ))
        })
    }
}

/// A request body whose first bytes were already read.
struct Prefixed {
    prefix: Option<Bytes>,
    inner: hyper::body::Incoming,
}

impl Body for Prefixed {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<BodyFrame<Bytes>, hyper::Error>>> {
        if let Some(prefix) = self.prefix.take()
            && !prefix.is_empty()
        {
            return Poll::Ready(Some(Ok(BodyFrame::data(prefix))));
        }
        Pin::new(&mut self.inner).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.prefix.as_ref().is_none_or(Bytes::is_empty) && self.inner.is_end_stream()
    }
}

/// The request body (server to deployment), read through: its start frame
/// names the invocation and the attempt, and its replayed journal says where
/// the attempt's own commands start.
struct TapBody {
    inner: Prefixed,
    attempt: Arc<Mutex<AttemptState>>,
    serving: Arc<Serving>,
}

impl Body for TapBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<BodyFrame<Bytes>, hyper::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if let Poll::Ready(Some(Ok(frame))) = &polled
            && let Some(data) = frame.data_ref()
        {
            let mut state = self.attempt.lock().unwrap_or_else(PoisonError::into_inner);
            state.request.push(data);
            while let Ok(Some(frame)) = state.request.next_frame() {
                if frame.ty == MessageType::Start {
                    if let Ok(start) = frame.decode::<StartMessage>() {
                        state.key = (!start.key.is_empty()).then(|| start.key.clone());
                        state.known_entries = Some(start.known_entries);
                        let mut attempts = self
                            .serving
                            .attempts
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner);
                        let count = attempts.entry(start.id.clone()).or_insert(0);
                        *count += 1;
                        state.attempt = *count;
                    }
                    continue;
                }
                let Some(known) = state.known_entries else {
                    continue;
                };
                if state.replayed < known {
                    state.replayed += 1;
                    if frame.ty.is_command() {
                        state.record_command(frame.ty, &frame.payload);
                    }
                }
            }
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

/// The response body (deployment to server), passed on frame by frame after
/// each is matched against the crash plan. The frame that matches, and
/// everything after it, never leaves: the deployment dies there.
pub struct CutBody {
    inner: Option<restate_sdk::endpoint::ResponseBody>,
    attempt: Option<Arc<Mutex<AttemptState>>>,
    serving: Arc<Serving>,
    queued: std::collections::VecDeque<Bytes>,
}

impl CutBody {
    /// Split whole frames out of what the SDK wrote, crash-checking each.
    /// Answers the crashed target when a frame matched.
    fn admit(&mut self, data: &Bytes) -> Option<String> {
        let attempt = self.attempt.as_ref()?;
        let mut state = attempt.lock().unwrap_or_else(PoisonError::into_inner);
        if state.cut {
            return None;
        }
        state.response.push(data);
        state.response_raw.extend_from_slice(data);
        loop {
            let frame = match state.response.next_frame() {
                Ok(Some(frame)) => frame,
                // A frame this decoder cannot read is passed on untouched:
                // the server judges it, not the crash plan.
                Ok(None) | Err(_) => break,
            };
            let raw = state
                .response_raw
                .split_to(8 + frame.payload.len())
                .freeze();
            let (run_name, run_index) = match frame.ty {
                MessageType::RunCommand => (
                    frame.decode::<RunCommandMessage>().ok().map(|run| run.name),
                    None,
                ),
                MessageType::ProposeRunCompletion => frame
                    .decode::<ProposeRunCompletionMessage>()
                    .ok()
                    .and_then(|proposal| state.runs.get(&proposal.result_completion_id).cloned())
                    .map_or((None, None), |(name, index)| (Some(name), Some(index))),
                _ => (None, None),
            };
            let site = CrashSite {
                service: state.service.clone(),
                handler: state.handler.clone(),
                key: state.key.clone(),
                ty: frame.ty,
                command_index: state.commands,
                run_name,
                run_index,
                attempt: state.attempt,
            };
            let crashed = self
                .serving
                .plan
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .should_crash(&site, 0);
            if crashed {
                state.cut = true;
                self.queued.clear();
                return Some(state.target());
            }
            if frame.ty.is_command() {
                state.record_command(frame.ty, &frame.payload);
            }
            self.queued.push_back(raw);
        }
        if state.response.next_frame().is_err() {
            // Undecodable: hand the rest over as it is.
            let rest = state.response_raw.split().freeze();
            state.response = FrameDecoder::default();
            self.queued.push_back(rest);
        }
        None
    }
}

impl Body for CutBody {
    type Data = Bytes;
    type Error = BoxError;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<BodyFrame<Bytes>, BoxError>>> {
        if let Some(attempt) = &self.attempt {
            let mut state = attempt.lock().unwrap_or_else(PoisonError::into_inner);
            if state.held {
                drop(state);
                // A hold reset the attempt: the handler goes with the body.
                self.inner = None;
                self.queued.clear();
                return Poll::Ready(Some(Err("a hold reset this attempt".into())));
            }
            state.waker = Some(cx.waker().clone());
        }
        loop {
            if let Some(bytes) = self.queued.pop_front() {
                return Poll::Ready(Some(Ok(BodyFrame::data(bytes))));
            }
            let Some(inner) = self.inner.as_mut() else {
                return Poll::Ready(None);
            };
            match std::task::ready!(Pin::new(inner).poll_frame(cx)) {
                None => {
                    self.inner = None;
                    return Poll::Ready(None);
                }
                Some(Err(error)) => {
                    self.inner = None;
                    return Poll::Ready(Some(Err(error)));
                }
                Some(Ok(frame)) => {
                    let Ok(data) = frame.into_data() else {
                        continue;
                    };
                    if self.attempt.is_none() {
                        return Poll::Ready(Some(Ok(BodyFrame::data(data))));
                    }
                    if let Some(target) = self.admit(&data) {
                        // The deployment dies at this frame: the handler goes
                        // with the body, and the stream is reset.
                        self.inner = None;
                        self.serving.crash(&target);
                        return Poll::Ready(Some(Err(format!(
                            "the deployment died at a crash point of `{target}`"
                        )
                        .into())));
                    }
                }
            }
        }
    }
}
