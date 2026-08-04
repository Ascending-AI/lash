//! Session-lease observability harness (`runbooks/session-lease-triage`).
//!
//! Three phases, one process each, driven by
//! `scripts/session-lease-triage-e2e.sh`. Each phase induces one of the three
//! situations the published triage procedure claims to distinguish, then records
//! both surfaces an operator is told to consult: the `session_lease_diagnostics`
//! reading and the `session_execution_lease.*` trace timeline.
//!
//! * `hang`: a real turn parked inside a provider call that never returns. A
//!   second `LashCore` reads the lane while the turn is in flight and must see a
//!   named holder whose renewals are landing. Releasing the provider lets the
//!   turn commit, after which the lane reads as unheld and no lease event fired.
//! * `takeover`: the same parked turn, but its durable lane is swept by a peer
//!   mid-turn. The displaced holder's renewal is then rejected, so the trace must
//!   carry `renew_failed` followed by `taken_over` naming the successor at a
//!   higher generation. Releasing the provider afterwards records what actually
//!   happened to the turn, which is the claim the docs stake the most on: a lost
//!   lease is not a failed turn.
//! * `livelock`: the cause the procedure names for repeated CAS rejections: two
//!   concurrent writers on one session under a single explicit
//!   `session_execution_owner`, so the second reenters the first's lease instead
//!   of being rejected as busy, and the loser's commit dies on the head CAS.
//!
//! Fault injection is deliberate and uses only public store surface. The sweep in
//! the `takeover` phase reads the live holder out of a busy claim outcome (which
//! carries the observed lease by contract, so a claimant can reclaim exactly what
//! it saw), releases it under that fence, and claims it for a named successor.
//! That is the peer's own mechanism, run on demand instead of after a TTL, so the
//! events the phase judges come from the production renewal path unchanged.
//!
//! Every phase runs against each configured backend (SQLite always, PostgreSQL
//! when `LASH_POSTGRES_DATABASE_URL` is set) and prints one JSON `checkpoint`
//! line per backend. Session ids carry a per-run suffix, so a shared PostgreSQL
//! database never collides with an earlier run and no phase truncates tables.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use lash::persistence::{
    LeaseOwnerIdentity, RuntimePersistence, SessionLeaseDiagnostics, SessionLeaseRenewal,
    SessionStoreCreateRequest, SessionStoreFactory,
};
use lash_postgres_store::PostgresStorage;
use serde_json::{Value, json};
use tracing_subscriber::layer::{Context as LayerContext, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

const TURN_PROMPT: &str = "commit one turn";
const SCRIPTED_PROGRAM: &str = "<lashlang>\nfinish \"ok\"\n</lashlang>";
/// Renewals land often enough that a healthy holder is provably renewing within
/// a phase, and the TTL is long enough that it never lapses on its own: every
/// lease loss in this harness is injected, never a timing accident.
const RENEW_INTERVAL: Duration = Duration::from_millis(50);
const LEASE_TTL: Duration = Duration::from_millis(5_000);
const GATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Timings for the two phases that watch the renewal loop work.
fn observable_timings() -> lash::durability::LeaseTimings {
    lash::durability::LeaseTimings::new(LEASE_TTL, RENEW_INTERVAL).expect("ttl >= 3 * renew")
}

/// Default timings for the livelock phase. The race it stages settles in
/// milliseconds, so a renewal must not fire inside it: the discriminator under
/// test is a rejected commit with `lease_lost = false`, and a renewal landing
/// mid-race would blur that into a handoff.
fn quiet_timings() -> lash::durability::LeaseTimings {
    lash::durability::LeaseTimings::default()
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let phase = std::env::args()
        .nth(1)
        .context("usage: lash-e2e-session-lease-triage hang|takeover|livelock")?;
    let capture = LeaseTraceCapture::install();
    let run_id = uuid::Uuid::new_v4().simple().to_string();

    for backend in Backend::configured().await? {
        let checkpoint = match phase.as_str() {
            "hang" => provider_hang(&backend, &capture, &run_id).await?,
            "takeover" => lease_takeover(&backend, &capture, &run_id).await?,
            "livelock" => commit_cas_livelock(&backend, &capture, &run_id).await?,
            other => bail!("unknown session-lease-triage phase `{other}`"),
        };
        emit(checkpoint);
    }
    Ok(())
}

fn emit(checkpoint: Value) {
    println!(
        "{}",
        serde_json::to_string(&checkpoint).expect("serialize checkpoint")
    );
}

// ---------------------------------------------------------------------------
// Trace capture
// ---------------------------------------------------------------------------

/// Collects the `session_execution_lease.*` trace events an operator is told to
/// read, in emission order, with every structured field.
///
/// This is the runbook's copy of the log an operator would grep, and the
/// *order* is part of the evidence: `renew_failed` before `taken_over` is what
/// tells a handoff apart from a transient store error.
#[derive(Clone, Default)]
struct LeaseTraceCapture {
    events: Arc<Mutex<Vec<Value>>>,
}

impl LeaseTraceCapture {
    fn install() -> Self {
        let capture = Self::default();
        let subscriber = Registry::default().with(capture.clone());
        tracing::subscriber::set_global_default(subscriber)
            .expect("install the lease trace capture");
        capture
    }

    fn reset(&self) {
        self.events.lock().expect("lock lease trace").clear();
    }

    fn timeline(&self) -> Vec<Value> {
        self.events.lock().expect("lock lease trace").clone()
    }

    fn named(&self, event: &str) -> Vec<Value> {
        self.timeline()
            .into_iter()
            .filter(|value| value.get("event").and_then(Value::as_str) == Some(event))
            .collect()
    }

    fn position_of(&self, event: &str) -> Option<usize> {
        self.timeline()
            .iter()
            .position(|value| value.get("event").and_then(Value::as_str) == Some(event))
    }

    /// Wait until `event` has been emitted, or fail the phase. Polling the
    /// captured timeline is how this harness gates on an asynchronous renewal
    /// loop instead of sleeping a fixed budget and hoping.
    async fn await_event(&self, event: &str, timeout: Duration) -> Result<Value> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(first) = self.named(event).into_iter().next() {
                return Ok(first);
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "trace event `{event}` was not emitted within {timeout:?}; timeline: {:?}",
                    self.timeline()
                );
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
}

struct FieldVisitor(BTreeMap<String, Value>);

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        // `Option` fields render as `Some("x")` or `None`; unwrap the payload so
        // an artifact reads the value rather than the wrapper.
        let rendered = format!("{value:?}");
        let unwrapped = rendered
            .strip_prefix("Some(")
            .and_then(|rest| rest.strip_suffix(')'))
            .map(|inner| inner.trim_matches('"').to_string())
            .unwrap_or_else(|| rendered.trim_matches('"').to_string());
        self.0
            .insert(field.name().to_string(), Value::from(unwrapped));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.0.insert(field.name().to_string(), Value::from(value));
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.0.insert(field.name().to_string(), Value::from(value));
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.0.insert(field.name().to_string(), Value::from(value));
    }
}

impl<S: tracing::Subscriber> Layer<S> for LeaseTraceCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _context: LayerContext<'_, S>) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        let Some(name) = visitor.0.get("event").and_then(Value::as_str) else {
            return;
        };
        if !name.starts_with("session_execution_lease.") {
            return;
        }
        let mut captured = json!({ "level": event.metadata().level().to_string() });
        for (key, value) in visitor.0 {
            captured[key] = value;
        }
        self.events.lock().expect("lock lease trace").push(captured);
    }
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// One durable substrate under test. The triage surfaces are backend-neutral by
/// contract, so every phase runs on each configured backend and the runner
/// requires the same observations from both.
struct Backend {
    name: &'static str,
    factory: Arc<dyn SessionStoreFactory>,
    /// Held so the SQLite root outlives the phase.
    _scratch: Option<tempfile::TempDir>,
    postgres: Option<PostgresStorage>,
}

impl Backend {
    async fn configured() -> Result<Vec<Self>> {
        let mut backends = vec![Self::sqlite()?];
        match std::env::var("LASH_POSTGRES_DATABASE_URL") {
            Ok(url) if !url.trim().is_empty() => backends.push(Self::postgres(&url).await?),
            _ => eprintln!(
                "session-lease-triage: LASH_POSTGRES_DATABASE_URL is unset; running SQLite only"
            ),
        }
        Ok(backends)
    }

    fn sqlite() -> Result<Self> {
        let scratch = tempfile::tempdir().context("scratch dir for the SQLite backend")?;
        let factory: Arc<dyn SessionStoreFactory> = Arc::new(
            lash_sqlite_store::SqliteSessionStoreFactory::new(scratch.path().join("sessions")),
        );
        Ok(Self {
            name: "sqlite",
            factory,
            _scratch: Some(scratch),
            postgres: None,
        })
    }

    async fn postgres(database_url: &str) -> Result<Self> {
        let storage = PostgresStorage::connect(database_url)
            .await
            .context("connect the PostgreSQL backend")?;
        let factory: Arc<dyn SessionStoreFactory> =
            Arc::new(storage.session_store_factory_with_shared_process_registry());
        Ok(Self {
            name: "postgres",
            factory,
            _scratch: None,
            postgres: Some(storage),
        })
    }

    /// A core wired for one deterministic turn. `owner` is the explicit
    /// session-execution identity; the livelock phase depends on two writers
    /// sharing one, which is exactly the misconfiguration the docs name.
    fn core(
        &self,
        provider: lash::provider::ProviderHandle,
        owner: Option<LeaseOwnerIdentity>,
        timings: lash::durability::LeaseTimings,
    ) -> Result<TurnCore> {
        let attachments = tempfile::tempdir().context("attachment dir for a triage turn")?;
        let artifacts: Arc<dyn lash::persistence::LashlangArtifactStore> = match &self.postgres {
            Some(storage) => Arc::new(storage.lashlang_artifact_store()),
            None => Arc::new(lash::persistence::InMemoryLashlangArtifactStore::default()),
        };
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::default(),
            artifacts,
        );
        let core = lash::LashCore::rlm_builder(factory)
            .provider(provider)
            .model(
                lash::ModelSpec::from_token_limits(
                    "session-lease-triage-mock",
                    Default::default(),
                    200_000,
                    None,
                )
                .map_err(anyhow::Error::msg)?,
            )
            .store_factory(Arc::clone(&self.factory))
            .attachment_store(Arc::new(lash::persistence::FileAttachmentStore::new(
                attachments.path().to_path_buf(),
            )))
            .process_env_store(match &self.postgres {
                Some(storage) => Arc::new(storage.process_env_store()),
                None => Arc::new(lash::persistence::InMemoryProcessExecutionEnvStore::default()),
            })
            .lease_timings(timings)
            .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
            .build()
            .context("build a session-lease-triage core")?;
        Ok(TurnCore {
            core,
            owner,
            _attachments: attachments,
        })
    }

    /// The durable store for one session, opened without creating it.
    async fn store(&self, session_id: &str) -> Result<Arc<dyn RuntimePersistence>> {
        self.factory
            .open_existing_store(&request(session_id))
            .await
            .map_err(anyhow::Error::msg)
            .with_context(|| format!("open the durable store for `{session_id}`"))?
            .with_context(|| format!("session `{session_id}` has no durable store"))
    }
}

struct TurnCore {
    core: lash::LashCore,
    owner: Option<LeaseOwnerIdentity>,
    _attachments: tempfile::TempDir,
}

impl TurnCore {
    async fn open(&self, session_id: &str) -> Result<lash::LashSession> {
        let mut builder = self.core.session(session_id);
        if let Some(owner) = self.owner.clone() {
            builder = builder.session_execution_owner(owner);
        }
        builder
            .open()
            .await
            .with_context(|| format!("open session `{session_id}`"))
    }
}

fn request(session_id: &str) -> SessionStoreCreateRequest {
    SessionStoreCreateRequest {
        session_id: session_id.to_string(),
        relation: lash::persistence::SessionRelation::default(),
        policy: lash::runtime::SessionPolicy::default(),
    }
}

fn owner(owner_id: &str, incarnation: &str) -> LeaseOwnerIdentity {
    LeaseOwnerIdentity::opaque(owner_id, incarnation)
}

/// Session ids are single-use (ADR 0049), so every run and backend gets its own.
fn session_id(phase: &str, backend: &Backend, run_id: &str) -> String {
    format!("lease-triage-{phase}-{}-{run_id}", backend.name)
}

fn scripted_provider() -> lash::provider::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("session-lease-triage")
        .complete(|_request| async { Ok(scripted_response()) })
        .build()
        .into_handle()
}

fn scripted_response() -> lash::provider::LlmResponse {
    lash::provider::LlmResponse {
        full_text: SCRIPTED_PROGRAM.to_string(),
        parts: vec![lash_core::LlmOutputPart::Text {
            text: SCRIPTED_PROGRAM.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..lash::provider::LlmResponse::default()
    }
}

/// A provider that parks forever, plus the handles to observe and release it.
struct StallingProvider {
    handle: lash::provider::ProviderHandle,
    entered: Arc<tokio::sync::Notify>,
    release: Arc<tokio::sync::Semaphore>,
}

impl StallingProvider {
    fn new() -> Self {
        let entered = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let handle = {
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            lash_core::testing::TestProvider::builder()
                .kind("session-lease-triage-stall")
                .complete(move |_request| {
                    let entered = Arc::clone(&entered);
                    let release = Arc::clone(&release);
                    async move {
                        entered.notify_waiters();
                        // Park the way a provider call with no timeout does. The
                        // renewal loop keeps the lane alive underneath.
                        release
                            .acquire()
                            .await
                            .expect("the stalling provider is released")
                            .forget();
                        Ok(scripted_response())
                    }
                })
                .build()
                .into_handle()
        };
        Self {
            handle,
            entered,
            release,
        }
    }

    async fn wait_until_parked(&self) -> Result<()> {
        let parked = self.entered.notified();
        tokio::time::timeout(GATE_TIMEOUT, parked)
            .await
            .context("the stalling provider was never entered")
    }

    fn release(&self) {
        self.release.add_permits(1);
    }
}

/// The reading, flattened the way the runbook scores it.
fn reading(diagnostics: Option<&SessionLeaseDiagnostics>) -> Value {
    let Some(diagnostics) = diagnostics else {
        return json!({ "session": "absent" });
    };
    let (renewal, expires_in_ms, expired_for_ms) = match diagnostics.renewal() {
        SessionLeaseRenewal::Unheld => ("unheld", None, None),
        SessionLeaseRenewal::Current { expires_in_ms } => ("current", Some(expires_in_ms), None),
        SessionLeaseRenewal::Lapsed { expired_for_ms } => ("lapsed", None, Some(expired_for_ms)),
    };
    let holder = diagnostics.holder.as_ref();
    json!({
        "session_id": diagnostics.session_id,
        "observed_at_epoch_ms": diagnostics.observed_at_epoch_ms,
        "renewal": renewal,
        "expires_in_ms": expires_in_ms,
        "expired_for_ms": expired_for_ms,
        "holder_owner_id": holder.map(|holder| holder.owner.owner_id.clone()),
        "holder_incarnation_id": holder.map(|holder| holder.owner.incarnation_id.clone()),
        "generation": holder.map(|holder| holder.generation),
        "claimed_at_epoch_ms": holder.map(|holder| holder.claimed_at_epoch_ms),
        "expires_at_epoch_ms": holder.map(|holder| holder.expires_at_epoch_ms),
    })
}

// ---------------------------------------------------------------------------
// Phase: provider hang
// ---------------------------------------------------------------------------

/// A turn parked in a provider call that never returns, read from outside.
///
/// This is the one situation whose evidence is *the absence* of lease trouble:
/// the lane is healthy and renewals keep landing, which is what redirects an
/// operator to the provider rather than the lease.
async fn provider_hang(
    backend: &Backend,
    capture: &LeaseTraceCapture,
    run_id: &str,
) -> Result<Value> {
    capture.reset();
    let session_id = session_id("hang", backend, run_id);
    let holder = owner("triage-hang-worker", "triage-hang-worker:boot-1");
    let provider = StallingProvider::new();

    let running = backend.core(
        provider.handle.clone(),
        Some(holder.clone()),
        observable_timings(),
    )?;
    let session = running.open(&session_id).await?;
    let turn = tokio::spawn(async move {
        session
            .turn(lash::TurnInput::text(TURN_PROMPT))
            .turn_id("lease-triage-hang-turn".to_string())
            .run()
            .await
    });
    provider.wait_until_parked().await?;

    // A second core sharing only the durable store: the operator's vantage
    // point, not the running worker's.
    let observer = backend.core(scripted_provider(), None, quiet_timings())?;
    let claimed = capture
        .await_event("session_execution_lease.claimed", GATE_TIMEOUT)
        .await?;
    // Gate on a landed renewal, so `Current` is evidence of a live renewal loop
    // rather than of the original claim's headroom.
    let renewed = capture
        .await_event("session_execution_lease.renewed", GATE_TIMEOUT)
        .await?;
    let parked = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;
    let holder_matches = parked
        .as_ref()
        .and_then(|diagnostics| diagnostics.holder.as_ref())
        .is_some_and(|observed| observed.owner == holder);
    let renewals_current = parked.as_ref().is_some_and(|diagnostics| {
        matches!(diagnostics.renewal(), SessionLeaseRenewal::Current { .. })
    });

    provider.release();
    let output = tokio::time::timeout(GATE_TIMEOUT, turn)
        .await
        .context("the released turn did not settle")?
        .context("the turn task panicked")?
        .map_err(anyhow::Error::msg)?;
    let committed = output.final_value() == Some(&json!("ok"));

    let after_commit = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;

    Ok(json!({
        "checkpoint": "provider_hang_shape",
        "backend": backend.name,
        "session_id": session_id,
        "expected_holder_owner_id": holder.owner_id,
        "claimed_generation": claimed.get("generation"),
        "renewed_generation": renewed.get("generation"),
        "holder_matches_running_worker": holder_matches,
        "renewals_current_while_parked": renewals_current,
        "reading_while_parked": reading(parked.as_ref()),
        "reading_after_commit": reading(after_commit.as_ref()),
        "turn_committed_after_release": committed,
        "renew_failed_count": capture.named("session_execution_lease.renew_failed").len(),
        "taken_over_count": capture.named("session_execution_lease.taken_over").len(),
        "commit_cas_rejected_count": capture
            .named("session_execution_lease.commit_cas_rejected")
            .len(),
        "lease_trace": capture.timeline(),
    }))
}

// ---------------------------------------------------------------------------
// Phase: takeover
// ---------------------------------------------------------------------------

/// A parked turn whose lane is swept out from under it by a peer.
///
/// The sweep uses the peer's own mechanism (observe the busy holder, release it
/// under that fence, claim it) rather than waiting out a TTL, so the events come
/// from the production renewal path with no timing luck involved. What the turn
/// then does is the point: the docs claim a lost lease is not a failed turn, and
/// this phase records the answer instead of asserting it.
async fn lease_takeover(
    backend: &Backend,
    capture: &LeaseTraceCapture,
    run_id: &str,
) -> Result<Value> {
    capture.reset();
    let session_id = session_id("takeover", backend, run_id);
    let displaced = owner("triage-displaced-worker", "triage-displaced-worker:boot-1");
    let successor = owner("triage-successor-worker", "triage-successor-worker:boot-1");
    let provider = StallingProvider::new();

    let running = backend.core(
        provider.handle.clone(),
        Some(displaced.clone()),
        observable_timings(),
    )?;
    let session = running.open(&session_id).await?;
    let turn = tokio::spawn(async move {
        session
            .turn(lash::TurnInput::text(TURN_PROMPT))
            .turn_id("lease-triage-takeover-turn".to_string())
            .run()
            .await
    });
    provider.wait_until_parked().await?;

    let observer = backend.core(scripted_provider(), None, quiet_timings())?;
    capture
        .await_event("session_execution_lease.claimed", GATE_TIMEOUT)
        .await?;
    let before = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;

    // Observe the live holder the way a peer claimant does: a busy outcome
    // carries the exact lease it lost to.
    let store = backend.store(&session_id).await?;
    let observed = match store
        .try_claim_session_execution_lease(&session_id, &successor, LEASE_TTL.as_millis() as u64)
        .await
        .map_err(anyhow::Error::msg)
        .context("probe the lane held by the parked turn")?
    {
        lash::persistence::SessionExecutionLeaseClaimOutcome::Busy { holder } => holder,
        lash::persistence::SessionExecutionLeaseClaimOutcome::Acquired(_) => {
            bail!("the parked turn was not holding its lane; the phase has nothing to displace")
        }
    };
    // Sweep it: release under the observed fence, then claim as the successor.
    store
        .release_session_execution_lease(&observed.completion())
        .await
        .map_err(anyhow::Error::msg)
        .context("release the lane under the observed fence")?;
    let taken = store
        .try_claim_session_execution_lease(&session_id, &successor, LEASE_TTL.as_millis() as u64)
        .await
        .map_err(anyhow::Error::msg)
        .context("claim the swept lane as the successor")?
        .acquired()
        .context("a swept lane must be claimable")?;

    // The displaced holder's renewal loop now presents a fence the row no longer
    // honors; the production path reports it and names the successor.
    let renew_failed = capture
        .await_event("session_execution_lease.renew_failed", GATE_TIMEOUT)
        .await?;
    let taken_over = capture
        .await_event("session_execution_lease.taken_over", GATE_TIMEOUT)
        .await?;
    let order_ok = capture.position_of("session_execution_lease.renew_failed")
        < capture.position_of("session_execution_lease.taken_over");

    let after = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;

    provider.release();
    let settled = tokio::time::timeout(GATE_TIMEOUT, turn)
        .await
        .context("the displaced turn never settled")?
        .context("the turn task panicked")?;
    let (turn_committed, turn_error) = match &settled {
        Ok(output) => (output.final_value() == Some(&json!("ok")), None),
        Err(err) => (false, Some(err.to_string())),
    };
    let rejections = capture.named("session_execution_lease.commit_cas_rejected");

    Ok(json!({
        "checkpoint": "lease_takeover",
        "backend": backend.name,
        "session_id": session_id,
        "displaced_owner_id": displaced.owner_id,
        "successor_owner_id": successor.owner_id,
        "displaced_generation": observed.fencing_token,
        "superseding_generation": taken.fencing_token,
        "renew_failed_before_taken_over": order_ok,
        "renew_failed": renew_failed,
        "taken_over": taken_over,
        "reading_before_takeover": reading(before.as_ref()),
        "reading_after_takeover": reading(after.as_ref()),
        "turn_committed_after_lease_loss": turn_committed,
        "turn_error_after_lease_loss": turn_error,
        "commit_cas_rejected_count": rejections.len(),
        "commit_cas_rejected": rejections,
        "lease_trace": capture.timeline(),
    }))
}

// ---------------------------------------------------------------------------
// Phase: livelock
// ---------------------------------------------------------------------------

/// Two writers on one session under a single explicit owner identity.
///
/// The docs name this as the cause of repeated CAS rejections, and it is the
/// case where the lease cannot help by construction: sharing an identity means
/// the second writer reenters the first's lease instead of seeing
/// `session_execution_busy`, so only the head compare-and-set stops it. Both
/// writers open before either commits, which is the state a host reaches by
/// routing two requests for one session to two writers that share an identity.
async fn commit_cas_livelock(
    backend: &Backend,
    capture: &LeaseTraceCapture,
    run_id: &str,
) -> Result<Value> {
    capture.reset();
    let session_id = session_id("livelock", backend, run_id);
    let shared = owner("triage-shared-worker", "triage-shared-worker:boot-1");

    let first = backend.core(scripted_provider(), Some(shared.clone()), quiet_timings())?;
    let second = backend.core(scripted_provider(), Some(shared.clone()), quiet_timings())?;
    let first_session = first.open(&session_id).await?;
    let second_session = second.open(&session_id).await?;

    // Race them. Sharing an identity means neither is rejected as busy, so both
    // reach the commit with the same expected head revision and the CAS is the
    // only thing that can separate them.
    let left = tokio::spawn(async move {
        first_session
            .turn(lash::TurnInput::text(TURN_PROMPT))
            .turn_id("lease-triage-livelock-left".to_string())
            .run()
            .await
    });
    let right = tokio::spawn(async move {
        second_session
            .turn(lash::TurnInput::text(TURN_PROMPT))
            .turn_id("lease-triage-livelock-right".to_string())
            .run()
            .await
    });
    let outcomes = [
        tokio::time::timeout(GATE_TIMEOUT, left)
            .await
            .context("the first racing turn never settled")?
            .context("the first turn task panicked")?,
        tokio::time::timeout(GATE_TIMEOUT, right)
            .await
            .context("the second racing turn never settled")?
            .context("the second turn task panicked")?,
    ];
    let committed = outcomes
        .iter()
        .filter(|outcome| {
            outcome
                .as_ref()
                .is_ok_and(|output| output.final_value() == Some(&json!("ok")))
        })
        .count();
    let winner_committed = committed == 1;
    let loser_error = outcomes
        .iter()
        .find_map(|outcome| outcome.as_ref().err().map(ToString::to_string));

    let rejections = capture.named("session_execution_lease.commit_cas_rejected");
    let observer = backend.core(scripted_provider(), None, quiet_timings())?;
    let after = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;

    Ok(json!({
        "checkpoint": "commit_cas_livelock",
        "backend": backend.name,
        "session_id": session_id,
        "shared_owner_id": shared.owner_id,
        "winner_committed": winner_committed,
        "loser_rejected": loser_error.is_some(),
        "loser_error": loser_error,
        "commit_cas_rejected_count": rejections.len(),
        "commit_cas_rejected": rejections,
        "renew_failed_count": capture.named("session_execution_lease.renew_failed").len(),
        "taken_over_count": capture.named("session_execution_lease.taken_over").len(),
        "reading_after_livelock": reading(after.as_ref()),
        "lease_trace": capture.timeline(),
    }))
}
