//! The Restate engine a crash-matrix world runs on: the in-process server
//! double, or a live `restate-server`.
//!
//! Every cell is written once, against [`Engine`]; the process environment
//! picks the engine for the whole run. `LASH_CRASH_MATRIX_ENGINE` unset (or
//! `double`) runs the double on virtual time. `LASH_CRASH_MATRIX_ENGINE=live`
//! runs each world against the server `RESTATE_INGRESS_URL` and
//! `RESTATE_ADMIN_URL` name, with the world's deployment serving its
//! endpoint on `LASH_CRASH_MATRIX_ENDPOINT_BIND` (and reached at
//! `LASH_CRASH_MATRIX_ENDPOINT_URL`, `http://<bind>` by default); `just
//! crash-matrix-restate-e2e` sets all of it. A live run holds one world at a
//! time on its server: the test binary runs with `--test-threads=1`.
//!
//! # How each fault lands on a live server
//!
//! A deployment kill is real on both engines: the deployment's host tasks
//! die, and every attempt the engine ran on it fails and is replayed into the
//! next deployment. On the double the server drops each running attempt; on
//! a live server the deployment's endpoint stops serving — its listener
//! closes and every connection and stream it served is dropped, so the
//! server sees each attempt's connection reset and retries it against the
//! restarted endpoint. A journal-step crash ([`CrashRule`]) is matched
//! against the same frames on both: the double drops the attempt before it
//! stores the frame; live, the deployment dies before the frame leaves it,
//! so the server never stores it either. Host-side crash sites are the same
//! decorators on both. The faults that differ:
//!
//! - **Holding an object's work** ([`Engine::hold_session_drive`],
//!   [`Engine::hold_service`]). The double keeps a held invocation's
//!   attempts from starting and suspends a running one at its next await. A
//!   live server has no such lever, so the deployment makes it with its own
//!   answers: it refuses every new attempt of a held invocation `503`, which
//!   the server backs off and retries, and resets a running one's stream.
//! - **Losing an invocation** is the admin API's kill on both.
//! - **Time.** The double's virtual clock moves the stores and the
//!   server's timers together. A live server's timers and retries run on
//!   wall time; the stores read wall time plus the offset the world's ticks
//!   and outages add, and each tick lands one jittered `T` after the last
//!   ([`Engine::advance_tick`]), so a lease, a claim or a due time passes at
//!   the tick it does on the double, and a detection bound is measured on
//!   the interval's own cadence.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use lash_core::{Clock, SessionWorkEngine};
use lash_restate_test::live::{LiveConfig, LiveRestateBackend};
use lash_restate_test::{CrashListener, CrashRule, HandlerAttempt, RestateTestBackend};

/// The handler host service the double's `run_in_handler` jobs run in: a
/// host's own call, which dies with the host rather than replaying.
pub(crate) const HANDLER_HOST: &str = "LashTestHandlerHost";

/// Where a live world's server and deployment are.
#[derive(Clone, Debug)]
pub struct LiveTarget {
    pub ingress_url: String,
    pub admin_url: String,
    pub endpoint_bind: SocketAddr,
    pub endpoint_url: String,
}

/// Which engine the run's worlds use.
#[derive(Clone, Debug)]
pub enum EngineKind {
    Double,
    Live(LiveTarget),
}

impl EngineKind {
    /// The engine the process environment names (see the module
    /// documentation). A live run missing an address is an error, never a
    /// silent fallback to the double.
    pub fn from_env() -> Result<Self, String> {
        let kind = std::env::var("LASH_CRASH_MATRIX_ENGINE").unwrap_or_default();
        match kind.as_str() {
            "" | "double" => Ok(Self::Double),
            "live" => {
                let var = |name: &str| {
                    std::env::var(name).map_err(|_| format!("a live crash-matrix run needs {name}"))
                };
                let endpoint_bind: SocketAddr = var("LASH_CRASH_MATRIX_ENDPOINT_BIND")?
                    .parse()
                    .map_err(|error| format!("LASH_CRASH_MATRIX_ENDPOINT_BIND: {error}"))?;
                let endpoint_url = std::env::var("LASH_CRASH_MATRIX_ENDPOINT_URL")
                    .unwrap_or_else(|_| format!("http://{endpoint_bind}"));
                Ok(Self::Live(LiveTarget {
                    ingress_url: var("RESTATE_INGRESS_URL")?,
                    admin_url: var("RESTATE_ADMIN_URL")?,
                    endpoint_bind,
                    endpoint_url,
                }))
            }
            other => Err(format!(
                "LASH_CRASH_MATRIX_ENGINE is `{other}`: expected `double` or `live`"
            )),
        }
    }
}

/// One invocation the engine holds, as the matrix reads it.
#[derive(Clone, Debug)]
pub struct EngineInvocation {
    pub id: String,
    /// `Service/key/handler`.
    pub target: String,
    /// Restate's lifecycle name (`running`, `suspended`, `completed`, ...).
    pub status: String,
    pub attempts: u64,
    pub last_failure: Option<String>,
}

/// See the module documentation.
#[derive(Clone, Debug)]
pub enum Engine {
    Double(RestateTestBackend),
    Live(LiveRestateBackend),
}

/// A hold on a service or one of its keys; [`release`](Self::release) lets
/// its invocations run again.
pub enum EngineHold {
    Double(lash_restate_test::Hold),
    Live(lash_restate_test::live::LiveHold),
}

impl EngineHold {
    pub fn release(self) {
        match self {
            Self::Double(hold) => hold.release(),
            Self::Live(hold) => hold.release(),
        }
    }
}

fn run_tag(seed: u64) -> String {
    static WORLDS: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    format!(
        "{seed:x}-{nanos:x}-{}",
        WORLDS.fetch_add(1, Ordering::SeqCst)
    )
}

impl Engine {
    /// A fresh engine for one world under `seed`.
    pub async fn start(kind: &EngineKind, seed: u64) -> Result<Self, String> {
        match kind {
            EngineKind::Double => {
                let mut config = lash_restate_test::ServerConfig::default();
                // A crashed attempt is retried at once: retry timing is no
                // contract, and a crash-matrix world crashes an attempt at
                // every cut.
                config.retry.initial_interval = Duration::from_millis(1);
                config.retry.max_interval = Duration::from_millis(10);
                lash_restate_test::backend(seed, config)
                    .await
                    .map(Self::Double)
                    .map_err(|error| format!("build the Restate test backend: {error}"))
            }
            EngineKind::Live(target) => LiveRestateBackend::start(LiveConfig {
                ingress_url: target.ingress_url.clone(),
                admin_url: target.admin_url.clone(),
                endpoint_bind: target.endpoint_bind,
                endpoint_url: target.endpoint_url.clone(),
                run_tag: run_tag(seed),
                namespace: lash_restate::RestateNamespace::default(),
            })
            .await
            .map(Self::Live)
            .map_err(|error| format!("bring up the live Restate backend: {error}")),
        }
    }

    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Double(_) => "double",
            Self::Live(_) => "live",
        }
    }

    /// The clock the stores stamp with.
    #[must_use]
    pub fn clock(&self) -> Arc<dyn Clock> {
        match self {
            Self::Double(double) => double.test_clock(),
            Self::Live(live) => live.clock(),
        }
    }

    /// Store time now, epoch milliseconds.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        match self {
            Self::Double(double) => double.server().now_ms(),
            Self::Live(live) => live.now_ms(),
        }
    }

    /// Let `by` of engine time pass: the double's virtual clock, whose
    /// timers fire; a live world's store clock.
    pub fn advance(&self, by: Duration) {
        match self {
            Self::Double(double) => {
                double.server().advance(by);
            }
            Self::Live(live) => live.advance(by),
        }
    }

    /// Move the clock for the recovery interval's next tick, `period` after
    /// its last at `last_tick_ms`, and answer the tick's time. Both engines'
    /// clocks also move between ticks: a live world's store clock flows with
    /// wall time, and the double's virtual clock flows at wall speed and
    /// fires the timers its idle server reaches — each including the
    /// harness's own waits for the engine to settle. So the clock moves only
    /// as far as `last_tick_ms + period`: the interval fires every `T`, as a
    /// deployment's does, whatever the harness spent, and a detection bound
    /// measured at the tick is the interval's, not the harness's.
    pub fn advance_tick(&self, last_tick_ms: u64, period: Duration) -> u64 {
        let due =
            last_tick_ms.saturating_add(u64::try_from(period.as_millis()).unwrap_or(u64::MAX));
        let now = self.now_ms();
        if due > now {
            self.advance(Duration::from_millis(due - now));
        }
        self.now_ms()
    }

    #[must_use]
    pub fn explicit_reconcile_session_work(&self) -> Arc<dyn SessionWorkEngine> {
        match self {
            Self::Double(double) => double.explicit_reconcile_session_work(),
            Self::Live(live) => live.explicit_reconcile_session_work(),
        }
    }

    #[must_use]
    pub fn lash_backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(double) => double.lash_backend(),
            Self::Live(live) => live.lash_backend(),
        }
    }

    pub fn install_process_worker(&self, worker: lash::durability::DurableProcessWorker) {
        match self {
            Self::Double(double) => double.install_process_worker(worker),
            Self::Live(live) => live.install_process_worker(worker),
        }
    }

    /// Call `listener` with the target of every attempt a journal-step crash
    /// dropped.
    pub fn on_crash(&self, listener: CrashListener) {
        match self {
            Self::Double(double) => {
                double.server().on_crash(listener);
            }
            Self::Live(live) => {
                live.on_crash(listener);
            }
        }
    }

    /// Arm a journal-step crash.
    pub fn crash_on(&self, rule: CrashRule) {
        match self {
            Self::Double(double) => double.server().crash_on(rule),
            Self::Live(live) => live.crash_on(rule),
        }
    }

    /// Run `attempt` inside a handler of the host's own on the engine.
    pub async fn run_in_handler(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: HandlerAttempt,
    ) -> Result<(), String> {
        match self {
            Self::Double(double) => double.run_in_handler(admitted, attempt).await,
            Self::Live(live) => live.run_in_handler(admitted, attempt).await,
        }
    }

    #[must_use]
    pub fn ingress(&self) -> lash_restate::RestateIngressClient {
        match self {
            Self::Double(double) => double.ingress(),
            Self::Live(live) => live.ingress(),
        }
    }

    /// Every invocation the engine holds. A live admin read that fails
    /// answers what it could not read as one `unreadable` row, so a check
    /// never passes on an empty read.
    pub async fn invocations(&self) -> Vec<EngineInvocation> {
        match self {
            Self::Double(double) => double
                .server()
                .invocations()
                .into_iter()
                .map(|view| EngineInvocation {
                    id: view.id,
                    target: view.target,
                    status: view.status.to_owned(),
                    attempts: u64::from(view.attempts),
                    last_failure: view
                        .last_failure
                        .map(|(code, message)| format!("{code}: {message}")),
                })
                .collect(),
            Self::Live(live) => match live.invocations().await {
                Ok(rows) => rows
                    .into_iter()
                    .map(|row| EngineInvocation {
                        id: row.id,
                        target: row.target,
                        status: row.status,
                        attempts: row.retry_count.unwrap_or_default() + 1,
                        last_failure: row.last_failure,
                    })
                    .collect(),
                Err(error) => vec![EngineInvocation {
                    id: String::new(),
                    target: format!("unreadable: {error}"),
                    status: "unreadable".to_owned(),
                    attempts: 0,
                    last_failure: None,
                }],
            },
        }
    }

    /// Kill `id` as an operator does and wait until it can run nothing more.
    pub async fn kill_and_await(&self, id: &str) -> Result<(), String> {
        match self {
            Self::Double(double) => {
                double.server().kill_and_await(id).await;
                Ok(())
            }
            Self::Live(live) => live
                .kill_and_await(id)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string()),
        }
    }

    /// How `id` completed; `None` while it has not.
    pub async fn outcome(&self, id: &str) -> Option<Result<(), String>> {
        match self {
            Self::Double(double) => double.server().outcome(id).map(|outcome| {
                outcome
                    .map(|_| ())
                    .map_err(|(code, message)| format!("{code}: {message}"))
            }),
            Self::Live(live) => match live.outcome(id).await {
                Ok(outcome) => outcome,
                Err(error) => Some(Err(format!("unreadable: {error}"))),
            },
        }
    }

    /// `id`'s journal commands, named.
    pub async fn journal_names(&self, id: &str) -> Vec<String> {
        match self {
            Self::Double(double) => double
                .server()
                .journal(id)
                .unwrap_or_default()
                .into_iter()
                .filter(|entry| entry.ty.is_command())
                .map(|entry| format!("{:?}:{}", entry.ty, entry.name.unwrap_or_default()))
                .collect(),
            Self::Live(live) => live
                .journal(id)
                .await
                .unwrap_or_else(|error| vec![format!("unreadable: {error}")]),
        }
    }

    /// Wait, at most `budget` of wall time, until the engine has nothing
    /// moving: every live attempt blocked on the server (the double), or two
    /// reads of every open invocation agreeing with none about to run (live).
    pub async fn settle(&self, budget: Duration) {
        match self {
            Self::Double(double) => {
                let _ = tokio::time::timeout(budget, double.server().settle()).await;
            }
            Self::Live(live) => live.settle(budget, Duration::from_millis(100)).await,
        }
    }

    /// Hold the engine's drive of `session`: no attempt of it runs until
    /// the hold is released.
    pub async fn hold_session_drive(&self, session: &lash_core::SessionId) -> EngineHold {
        match self {
            Self::Double(double) => EngineHold::Double(double.hold_session_drive(session).await),
            Self::Live(live) => EngineHold::Live(live.hold(
                lash_restate_test::SESSION_DRIVER_SERVICE,
                Some(session.as_str()),
            )),
        }
    }

    /// Hold every invocation of `service` until the hold is released.
    pub async fn hold_service(&self, service: &str) -> EngineHold {
        match self {
            Self::Double(double) => EngineHold::Double(double.server().hold_service(service).await),
            Self::Live(live) => EngineHold::Live(live.hold(service, None)),
        }
    }

    /// The deployment died where it stood: every attempt the engine ran on
    /// it fails — a lash invocation is replayed, a host's own handler job is
    /// not: it is killed.
    pub async fn kill_deployment(&self) {
        match self {
            Self::Double(double) => {
                let server = double.server();
                for view in server.invocations() {
                    if view.status != "running" {
                        continue;
                    }
                    if view.target.starts_with(HANDLER_HOST) {
                        let _ = server.kill_and_await(&view.id).await;
                    } else {
                        let _ = server.crash(&view.id);
                    }
                }
            }
            // The host's own jobs die with it, as on the double: the
            // backend drops their closures and the admin API kills their
            // invocations. A kill propagates to the calls a job waits on, so
            // a stage whose job must not take its callees with it holds them
            // until the job answered (the process stage holds its run).
            Self::Live(live) => {
                live.stop_serving(true);
                for view in self.invocations().await {
                    if view.status != "completed" && view.target.starts_with(HANDLER_HOST) {
                        let _ = live.kill_and_await(&view.id).await;
                    }
                }
            }
        }
    }

    /// Serve the next deployment's endpoint (live; the double's endpoint
    /// never stopped).
    pub async fn revive_deployment(&self) -> Result<(), String> {
        match self {
            Self::Double(_) => Ok(()),
            Self::Live(live) => live
                .start_serving()
                .await
                .map_err(|error| format!("serve the restarted deployment: {error}")),
        }
    }

    /// Release what the world holds on the engine: a live server outlives
    /// the world, so every invocation it still holds open is killed.
    pub async fn finish(&self) {
        if let Self::Live(live) = self {
            live.finish().await;
        }
    }
}
