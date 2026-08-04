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
//! * `takeover`: a lane left held by a worker that is gone, swept by a real turn.
//!   The winner's claim must emit `taken_over` naming the abandoned holder and a
//!   strictly higher generation, and the dead holder must emit nothing at all:
//!   that is the case a takeover reported from the loser's renewal path missed
//!   entirely.
//! * `livelock`: the cause the procedure names for repeated CAS rejections: two
//!   concurrent writers on one session under a single explicit
//!   `session_execution_owner`, so the second reenters the first's lease instead
//!   of being rejected as busy, and the loser's commit dies on the head CAS.
//!
//! Fault injection is deliberate and uses only public store surface. The
//! `takeover` phase seeds an abandoned lease row (TTL zero, claimed through the
//! store, so no guard and no renewal task exist behind it) and then lets a real
//! turn claim the session. Nothing is released on the dead worker's behalf and
//! nothing waits for it to notice, because it never will: that absence is the
//! point of the scenario.
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
/// Rounds of sustained misrouting. One collision is contention; the docs
/// diagnose recurrence, so the scenario has to show the cycle repeating.
const LIVELOCK_ROUNDS: usize = 3;

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
/// This is the runbook's copy of the log an operator would grep. Order is not the
/// evidence: `taken_over` comes from the winner's claim, so in the flagship case
/// it is the *only* lease event, the dead holder having emitted nothing. What
/// establishes a handoff is that a `taken_over` names a displaced holder at a
/// lower generation; a `session_execution_lease.lost` from that holder is a separate, optional
/// notice that appears only if it was still alive to file one.
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
                // Same provider kind as the scripted variant: a session records the
                // provider id it was created under, so a later core that registers
                // a different kind cannot drive it.
                .kind("session-lease-triage")
                .complete(move |_request| {
                    let entered = Arc::clone(&entered);
                    let release = Arc::clone(&release);
                    async move {
                        // `notify_one` stores a permit when no waiter has registered
                        // yet, so the harness cannot miss the parked signal by
                        // reaching the provider before the gate is awaited.
                        entered.notify_one();
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
        "fencing_token": holder.map(|holder| holder.generation),
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
        .await_event("session_execution_lease.acquired", GATE_TIMEOUT)
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
        "claimed_generation": claimed.get("fencing_token"),
        "renewed_generation": renewed.get("fencing_token"),
        "holder_matches_running_worker": holder_matches,
        "renewals_current_while_parked": renewals_current,
        "reading_while_parked": reading(parked.as_ref()),
        "reading_after_commit": reading(after_commit.as_ref()),
        "turn_committed_after_release": committed,
        "lease_lost_count": capture.named("session_execution_lease.lost").len(),
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

/// The flagship production takeover: a worker is gone, and a peer sweeps its lane.
///
/// The abandoned row is seeded through the store by an owner that has no guard and
/// no renewal task anywhere in this process, which is what a killed or frozen
/// worker actually leaves behind. Nothing that worker would have logged happens.
/// A real turn then claims the session, and the truthful takeover has to come from
/// that winner, atomically with its claim.
///
/// Deliberately not rigged: nothing releases the lane on the dead worker's behalf,
/// and nothing waits for it to notice, because in this case it never will. The
/// live-loser variant (a holder that is still running when its lane moves, which
/// additionally logs its own `session_execution_lease.lost`) is covered by the lash-core unit
/// tests; it is a strictly easier case and not the one that used to go unreported.
async fn lease_takeover(
    backend: &Backend,
    capture: &LeaseTraceCapture,
    run_id: &str,
) -> Result<Value> {
    capture.reset();
    let session_id = session_id("takeover", backend, run_id);
    let abandoned_by = owner("triage-dead-worker", "triage-dead-worker:boot-1");
    let successor = owner("triage-successor-worker", "triage-successor-worker:boot-1");

    // Materialize the session with one committed turn, so the lane belongs to a
    // real session rather than a bare row.
    let seed = backend.core(
        scripted_provider(),
        Some(successor.clone()),
        quiet_timings(),
    )?;
    let seed_session = seed.open(&session_id).await?;
    seed_session
        .turn(lash::TurnInput::text(TURN_PROMPT))
        .turn_id("lease-triage-takeover-seed".to_string())
        .run()
        .await
        .map_err(anyhow::Error::msg)?;
    drop(seed_session);

    // Leave the row held by a worker that is gone: TTL zero, claimed straight
    // through the store, so there is no guard and no renewal loop behind it.
    let store = backend.store(&session_id).await?;
    let abandoned = store
        .try_claim_session_execution_lease(&session_id, &abandoned_by, 0)
        .await
        .map_err(anyhow::Error::msg)
        .context("seed the abandoned lane")?
        .acquired()
        .context("an unheld lane is acquirable")?;
    capture.reset();

    let observer = backend.core(scripted_provider(), None, quiet_timings())?;
    let before = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;

    // A real turn sweeps the lane. Its claim is the takeover.
    let sweeper = backend.core(
        scripted_provider(),
        Some(successor.clone()),
        quiet_timings(),
    )?;
    let sweeper_session = sweeper.open(&session_id).await?;
    let swept = sweeper_session
        .turn(lash::TurnInput::text(TURN_PROMPT))
        .turn_id("lease-triage-takeover-sweep".to_string())
        .run()
        .await;
    let (turn_committed, turn_error) = match &swept {
        Ok(output) => (output.final_value() == Some(&json!("ok")), None),
        Err(err) => (false, Some(err.to_string())),
    };

    let taken_over = capture
        .await_event("session_execution_lease.taken_over", GATE_TIMEOUT)
        .await?;
    let after = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;
    let rejections = capture.named("session_execution_lease.commit_cas_rejected");

    Ok(json!({
        "checkpoint": "lease_takeover",
        "backend": backend.name,
        "session_id": session_id,
        "abandoned_owner_id": abandoned_by.owner_id,
        "successor_owner_id": successor.owner_id,
        "abandoned_generation": abandoned.fencing_token,
        "taken_over": taken_over,
        "taken_over_count": capture.named("session_execution_lease.taken_over").len(),
        // The dead holder runs nothing, so it reports nothing. This is precisely
        // the case a loser-emitted takeover event would have missed entirely.
        "lease_lost_count": capture.named("session_execution_lease.lost").len(),
        "reading_before_takeover": reading(before.as_ref()),
        "reading_after_takeover": reading(after.as_ref()),
        "turn_committed_after_takeover": turn_committed,
        "turn_error_after_takeover": turn_error,
        "commit_cas_rejected_count": rejections.len(),
        "commit_cas_rejected": rejections,
        "lease_trace": capture.timeline(),
    }))
}

// ---------------------------------------------------------------------------
// Phase: livelock
// ---------------------------------------------------------------------------

/// Sustained misrouting: two writers keep being handed the same session under one
/// explicit owner identity, and each retries after losing.
///
/// One collision is ordinary contention and proves nothing; the docs diagnose
/// *repeated* `commit_cas_rejected` with `lease_lost = false` as livelock, so the
/// harness has to produce recurrence. Sharing an identity is what makes it
/// recur: the second writer reenters the first's lease instead of being rejected
/// as busy, so nothing serializes the pair and every round collides again. Each
/// round the loser reloads and retries, exactly as a retry-on-conflict host does,
/// which is the shape that turns contention into a cycle.
async fn commit_cas_livelock(
    backend: &Backend,
    capture: &LeaseTraceCapture,
    run_id: &str,
) -> Result<Value> {
    capture.reset();
    let session_id = session_id("livelock", backend, run_id);
    let shared = owner("triage-shared-worker", "triage-shared-worker:boot-1");
    let mut rounds = Vec::new();

    for round in 0..LIVELOCK_ROUNDS {
        let before = capture
            .named("session_execution_lease.commit_cas_rejected")
            .len();
        // Both writers open before either commits, so both snapshot the same head
        // revision. Parking one inside its provider is what makes the overlap
        // deterministic: without it the two turns can serialize by scheduling luck
        // and the round proves nothing.
        let stalled_provider = StallingProvider::new();
        let stalled_core = backend.core(
            stalled_provider.handle.clone(),
            Some(shared.clone()),
            quiet_timings(),
        )?;
        let racer_core =
            backend.core(scripted_provider(), Some(shared.clone()), quiet_timings())?;
        let stalled_session = stalled_core.open(&session_id).await?;
        let racer_session = racer_core.open(&session_id).await?;

        let stalled = tokio::spawn(async move {
            stalled_session
                .turn(lash::TurnInput::text(TURN_PROMPT))
                .turn_id(format!("lease-triage-livelock-{round}-stalled"))
                .run()
                .await
        });
        if stalled_provider.wait_until_parked().await.is_err() {
            // The parked turn is the fixture; if it failed before reaching the
            // provider, report that failure rather than a bare timeout.
            let settled = tokio::time::timeout(Duration::from_secs(5), stalled).await;
            bail!("round {round}: the stalled turn never reached the provider: {settled:?}");
        }

        // The peer reenters the same lease (shared identity, so no busy rejection)
        // and publishes first.
        let racer = racer_session
            .turn(lash::TurnInput::text(TURN_PROMPT))
            .turn_id(format!("lease-triage-livelock-{round}-racer"))
            .run()
            .await;
        let racer_committed = racer
            .as_ref()
            .is_ok_and(|output| output.final_value() == Some(&json!("ok")));

        // Now let the parked writer try to publish against a head that moved.
        stalled_provider.release();
        let stalled = tokio::time::timeout(GATE_TIMEOUT, stalled)
            .await
            .with_context(|| format!("round {round}: the parked turn never settled"))?
            .with_context(|| format!("round {round}: the parked turn panicked"))?;
        let stalled_committed = stalled
            .as_ref()
            .is_ok_and(|output| output.final_value() == Some(&json!("ok")));
        let loser_error = stalled
            .as_ref()
            .err()
            .map(ToString::to_string)
            .or_else(|| racer.as_ref().err().map(ToString::to_string));
        let rejected_this_round = capture
            .named("session_execution_lease.commit_cas_rejected")
            .len()
            - before;
        rounds.push(json!({
            "round": round,
            "committed": usize::from(racer_committed) + usize::from(stalled_committed),
            "loser_rejected": loser_error.is_some(),
            "loser_error": loser_error,
            "commit_cas_rejected_in_round": rejected_this_round,
        }));
    }

    let rejections = capture.named("session_execution_lease.commit_cas_rejected");
    let observer = backend.core(scripted_provider(), None, quiet_timings())?;
    let after = observer
        .core
        .session_lease_diagnostics(&session_id)
        .await
        .map_err(anyhow::Error::msg)?;
    let rounds_with_a_rejection = rounds
        .iter()
        .filter(|round| round["commit_cas_rejected_in_round"].as_u64().unwrap_or(0) > 0)
        .count();

    Ok(json!({
        "checkpoint": "commit_cas_livelock",
        "backend": backend.name,
        "session_id": session_id,
        "shared_owner_id": shared.owner_id,
        "rounds_attempted": LIVELOCK_ROUNDS,
        "rounds_with_a_rejection": rounds_with_a_rejection,
        "rounds": rounds,
        "commit_cas_rejected_count": rejections.len(),
        "commit_cas_rejected": rejections,
        "lease_lost_count": capture.named("session_execution_lease.lost").len(),
        "taken_over_count": capture.named("session_execution_lease.taken_over").len(),
        "reading_after_livelock": reading(after.as_ref()),
        "lease_trace": capture.timeline(),
    }))
}
