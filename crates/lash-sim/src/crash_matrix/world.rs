//! What one run of a crash-matrix case shares across its nodes: the outside
//! world every body writes to, the replay tripwire, the deployment's
//! backends and the host that acts on the deployment from outside it.
//!
//! The outside world survives every node, as a real one would: a body that
//! a crash cannot take back is visible to the laws through the
//! [`BodyLedger`], keyed by owner and call. The host writes through its own
//! producer store ([`lash_durable_test::SimNodes::producer`]), so its mail,
//! cancels and resolves are labelled and cut like any node's writes.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use lash_core::sync::MutexExt as _;
use lash_core_execution::Backend;
use lash_durable::domain::OwnerKey;
use lash_durable::{ActorKey, DurableError, StoreFailureKind};
use lash_durable_test::{SimClock, SimNodes, Tripwire};
use lash_sansio::{ExecutionPolicy, ProcessId, SessionId, ToolCallId};

/// One body entry, noted before the body does anything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BodyEntry {
    /// The tool or operation the body runs.
    pub tool: String,
    /// The policy its admission pinned.
    pub policy: ExecutionPolicy,
    /// The attempt the body was handed, from 1.
    pub attempt: u32,
    /// Virtual time at entry.
    pub at_ms: u64,
    /// Whether the body found its `x_start` committed when it was entered.
    pub admitted: bool,
}

/// Every body entry of a run, per owner and call: the body ledger of
/// design-opus §7.2, which no node can rewrite.
#[derive(Debug, Default)]
pub struct BodyLedger {
    entries: Mutex<BTreeMap<(OwnerKey, ToolCallId), Vec<BodyEntry>>>,
}

impl BodyLedger {
    /// Note that `owner`'s body for `call` was entered.
    pub fn enter(&self, owner: &OwnerKey, call: &ToolCallId, entry: BodyEntry) {
        self.entries
            .lock_recover()
            .entry((owner.clone(), call.clone()))
            .or_default()
            .push(entry);
    }

    /// Every entry so far.
    pub fn entries(&self) -> BTreeMap<(OwnerKey, ToolCallId), Vec<BodyEntry>> {
        self.entries.lock_recover().clone()
    }

    /// Every entry of `tool`'s bodies, in entry order per call.
    pub fn of_tool(&self, tool: &str) -> Vec<((OwnerKey, ToolCallId), Vec<BodyEntry>)> {
        self.entries
            .lock_recover()
            .iter()
            .filter(|(_, entries)| entries.iter().any(|entry| entry.tool == tool))
            .map(|(key, entries)| (key.clone(), entries.clone()))
            .collect()
    }
}

/// The deployment's parts, built once the run has its database.
#[derive(Clone)]
struct Parts {
    backend: Backend,
    clock: Arc<SimClock>,
}

/// The note a soak epoch's world carries: its laws hold across many
/// faults, not one cut.
pub const SOAK: &str = "soak";

/// The name the host's producer store writes under.
pub const HOST: &str = "host";

/// The name the outside world's reads go through.
const READER: &str = "reader";

/// The host's parts, built once the run has its nodes.
#[derive(Clone)]
struct HostParts {
    backend: Backend,
    nodes: Weak<SimNodes>,
    /// The database as the outside world reads it: through a producer
    /// store, so the deployment's quiescence waits for every read in flight.
    reader: Arc<dyn lash_durable::DurableStore>,
}

/// The deployment's backend writing through a fresh boot of the host's
/// producer store.
fn host_backend(backend: &Backend, nodes: &SimNodes) -> Backend {
    let producer = nodes.producer(HOST);
    let stores = lash_core::testing::runtime_helpers::LayeredStores::over(backend.stores())
        .map_durable_store(move |_| producer)
        .into_store_set();
    backend.over_stores(stores)
}

/// What one run shares: built fresh for every cell.
#[derive(Default)]
pub struct World {
    tripwire: Arc<Tripwire>,
    /// The core the run's cell sessions run behind ([`super::cells`]).
    cell_core: OnceLock<lash::LashCore>,
    /// The core the run's prompt sessions run behind ([`super::prompts`]).
    prompt_core: OnceLock<lash::LashCore>,
    /// The core the run's compaction sessions run behind
    /// ([`super::compactions`]).
    compaction_core: OnceLock<lash::LashCore>,
    ledger: BodyLedger,
    parts: OnceLock<Parts>,
    host: Mutex<Option<HostParts>>,
    actors: Mutex<Vec<ActorKey>>,
    notes: Mutex<Vec<String>>,
    /// The process each session's tools address.
    targets: Mutex<BTreeMap<SessionId, ProcessId>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl World {
    /// The tripwire every activation reports to.
    pub fn tripwire(&self) -> &Arc<Tripwire> {
        &self.tripwire
    }

    /// The run's cell core, once built.
    pub(crate) fn cell_core(&self) -> &OnceLock<lash::LashCore> {
        &self.cell_core
    }

    /// The run's prompt core, once built.
    pub(crate) fn prompt_core(&self) -> &OnceLock<lash::LashCore> {
        &self.prompt_core
    }

    /// The run's compaction core, once built.
    pub(crate) fn compaction_core(&self) -> &OnceLock<lash::LashCore> {
        &self.compaction_core
    }

    /// The outside world's body ledger.
    pub fn ledger(&self) -> &BodyLedger {
        &self.ledger
    }

    pub(crate) fn set_parts(&self, backend: Backend, clock: Arc<SimClock>) {
        let _ = self.parts.set(Parts { backend, clock });
    }

    /// Start the host over `nodes`.
    ///
    /// # Errors
    ///
    /// The run has no database yet.
    pub(crate) fn start_host(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        let backend = host_backend(&self.backend()?, nodes);
        *self.host.lock_recover() = Some(HostParts {
            backend,
            nodes: Arc::downgrade(nodes),
            reader: nodes.producer(READER),
        });
        Ok(())
    }

    /// The host crashed: its supervisor starts a new boot of it, whose
    /// calls enter the store again.
    fn restart_host(&self) {
        let mut host = self.host.lock_recover();
        let Some(parts) = host.as_ref() else {
            return;
        };
        let (Some(nodes), Ok(backend)) = (parts.nodes.upgrade(), self.backend()) else {
            return;
        };
        let reader = Arc::clone(&parts.reader);
        let backend = host_backend(&backend, &nodes);
        *host = Some(HostParts {
            backend,
            nodes: Arc::downgrade(&nodes),
            reader,
        });
        drop(host);
        self.note("host restarted");
    }

    /// The deployment's backend: its unfaulted reads and the activations'
    /// ports.
    ///
    /// # Errors
    ///
    /// The run has no database yet.
    pub fn backend(&self) -> Result<Backend, String> {
        self.parts
            .get()
            .map(|parts| parts.backend.clone())
            .ok_or_else(|| "the run's database is not built".to_owned())
    }

    /// The run's virtual clock.
    ///
    /// # Errors
    ///
    /// The run has no database yet.
    pub fn clock(&self) -> Result<Arc<SimClock>, String> {
        self.parts
            .get()
            .map(|parts| Arc::clone(&parts.clock))
            .ok_or_else(|| "the run's clock is not built".to_owned())
    }

    /// The host's backend: the deployment's, writing through the host's
    /// producer store.
    ///
    /// # Errors
    ///
    /// The run's nodes are not started yet.
    pub fn host(&self) -> Result<Backend, String> {
        self.host
            .lock_recover()
            .as_ref()
            .map(|host| host.backend.clone())
            .ok_or_else(|| "the run's host is not built".to_owned())
    }

    /// The run's nodes, while the run lasts.
    pub fn nodes(&self) -> Option<Arc<SimNodes>> {
        self.host
            .lock_recover()
            .as_ref()
            .and_then(|host| host.nodes.upgrade())
    }

    /// Whether `owner`'s run records hold a committed `x_start` for `call`:
    /// what a body reads first, so a law can tell one that started before
    /// its admission committed.
    pub async fn admitted(&self, owner: &OwnerKey, call: &ToolCallId) -> bool {
        let reader = self
            .host
            .lock_recover()
            .as_ref()
            .map(|host| Arc::clone(&host.reader));
        let Some(reader) = reader else {
            return false;
        };
        reader.run_records(owner).await.is_ok_and(|rows| {
            rows.iter().any(|row| {
                row.kind == lash_durable::domain::RunRecordKind::XStart
                    && row.call.as_ref() == Some(call)
            })
        })
    }

    /// Virtual time now, or 0 before the clock is built.
    pub fn now_ms(&self) -> u64 {
        self.parts.get().map_or(0, |parts| parts.clock.logical_ms())
    }

    /// Name `actor` as one of the run's: the matrix moves a paused node on
    /// only once none of them is its own, and the invariants read each.
    pub fn track(&self, actor: ActorKey) {
        let mut actors = self.actors.lock_recover();
        if !actors.contains(&actor) {
            actors.push(actor);
        }
    }

    /// The run's actors.
    pub fn actors(&self) -> Vec<ActorKey> {
        self.actors.lock_recover().clone()
    }

    /// Record what the host or a body observed, for a law to read.
    pub fn note(&self, note: impl Into<String>) {
        self.notes.lock_recover().push(note.into());
    }

    /// Everything noted so far.
    pub fn notes(&self) -> Vec<String> {
        self.notes.lock_recover().clone()
    }

    /// Name `process` as the one `session`'s tools address.
    pub fn set_target(&self, session: SessionId, process: ProcessId) {
        self.targets.lock_recover().insert(session, process);
    }

    /// The process `session`'s tools address.
    pub fn target(&self, session: &SessionId) -> Option<ProcessId> {
        self.targets.lock_recover().get(session).cloned()
    }

    /// Whether `note` was recorded.
    pub fn noted(&self, note: &str) -> bool {
        self.notes.lock_recover().iter().any(|seen| seen == note)
    }

    /// Run `task` beside the deployment until [`Self::stop_tasks`].
    pub fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        self.tasks.lock_recover().push(tokio::spawn(task));
    }

    /// Stop every task [`Self::spawn`] started: the run is over. A task
    /// holds the world, so the run's owner stops them.
    pub fn stop_tasks(&self) {
        for task in self.tasks.lock_recover().drain(..) {
            task.abort();
        }
    }

    /// Sleep `delay` of virtual time.
    pub async fn sleep(&self, delay: Duration) {
        if let Some(parts) = self.parts.get() {
            lash_core::Clock::sleep(&*parts.clock, delay).await;
        }
    }
}

/// How often a host retries a transient failure, and how long it waits
/// between tries.
const HOST_ATTEMPTS: usize = 8;
const HOST_RETRY: Duration = Duration::from_millis(100);
/// How long a host call may take before the host counts itself crashed.
const HOST_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether a host's call failed in a way a real host retries: the store was
/// unavailable or contended, or the commit's answer was lost.
pub fn transient(error: &DurableError) -> bool {
    match error {
        DurableError::AckLost { .. } => true,
        DurableError::Store(failure) => matches!(
            failure.kind,
            StoreFailureKind::Unavailable | StoreFailureKind::Contended
        ),
        _ => false,
    }
}

/// Run a host's call on the current host boot, as a host does: a transient
/// failure is retried, and a call that never answers means the host died
/// (a cut killed its producer store), so its supervisor restarts it and the
/// new boot calls again. A lost answer is retried too, so the call must be
/// idempotent.
///
/// # Errors
///
/// The call's last failure, or the host's absence.
pub async fn retry<T, F, Fut>(world: &World, mut call: F) -> Result<T, String>
where
    F: FnMut(Backend) -> Fut,
    Fut: Future<Output = Result<T, DurableError>>,
{
    let mut attempt = 1;
    loop {
        let host = world.host()?;
        let answer = tokio::select! {
            answer = call(host) => Some(answer),
            () = world.sleep(HOST_TIMEOUT) => None,
        };
        match answer {
            None if attempt < HOST_ATTEMPTS => {
                attempt += 1;
                world.restart_host();
            }
            Some(Err(error)) if transient(&error) && attempt < HOST_ATTEMPTS => {
                attempt += 1;
                world.sleep(HOST_RETRY).await;
            }
            Some(answer) => return answer.map_err(|error| error.to_string()),
            None => return Err("the host never got an answer".to_owned()),
        }
    }
}

/// Poll `ready` every `every` of virtual time until it answers `Some`, for
/// at most `limit` polls.
pub async fn poll<T, F, Fut>(
    world: &World,
    every: Duration,
    limit: usize,
    mut ready: F,
) -> Option<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Option<T>>,
{
    for _ in 0..limit {
        if let Some(found) = ready().await {
            return Some(found);
        }
        world.sleep(every).await;
    }
    None
}
