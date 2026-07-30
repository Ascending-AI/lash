# Work-item wait cutover: coordination off the persistence contract

Status: approved plan, ready to execute (lash repo only)
Scope: the `lash` repository. The downstream `figments` changes are a separate
plan executed after this lands and a version bump reaches that repo.
Style: wholehog — implement the final end-state directly, delete superseded
paths in the same change, no shims, no compatibility layers, no flags.

---

## 0. TL;DR

Waiting for a work item's (durable process's) result is a coordination concern
and belongs to the durable-engine seam. Today it is implemented as "interrogate
the store": `ProcessRegistry` — otherwise a pure-state trait — carries two
blocking methods, `await_process` and `wait_event_after`, and every store
backend hand-rolls a sleep-poll loop for them (sqlite and postgres poll at a
fixed 50ms with an in-process `Notify` that only helps same-process
completions; the downstream SurrealDB impl blind-polls at 250ms). Measured
downstream cost: ~20–25 point reads per second per waiter, ~80k reads for one
53-session benchmark question.

This cutover:

1. Deletes `await_process` and `wait_event_after` from `ProcessRegistry`.
2. Adds the await-completion capability to the process-work seam:
   `ProcessWorkDriver` gains `await_terminal` / `await_event`, backed by a
   single core-owned default (`ProcessAwaiter`: in-process change hub +
   exponential-backoff point reads) and an optional engine-native terminal
   attach (`ProcessAttach`).
3. Implements the Restate-native attach: a synchronous ingress call to the
   already-shipped `LashProcessWorkflow/{process_id}/await_terminal` durable
   promise handler. Zero polling in Restate deployments.
4. Reroutes every wait callsite through the driver seam and unifies the
   triplicated `ProcessCommand` execution match (inline + sqlite store-backed +
   postgres store-backed) into one lash-core helper.
5. Moves conformance wait coverage from per-store tests to driver-level tests.

The persistence traits end pure-state. The store backends end with **zero**
wait/notify code.

---

## 1. Background: the leak, and what already exists

Read these files before starting; the plan references them by symbol.

### 1.1 The work-item model

"Work items" are durable process records (`ProcessRecord`), managed by the
`ProcessRegistry` trait (`crates/lash-core/src/runtime/process/registry.rs`,
doc-commented "Durability-neutral process registry"). `ObservedWorkItem`
(`crates/lash-core/src/runtime/process/observation.rs`) is the observation
wrapper. Process *execution* is owned by:

- **Engineless tier**: `DurableProcessWorker`
  (`crates/lash-core/src/runtime/process_worker.rs`) driven by
  `ProcessWorkDriver` + `InlineProcessRunHandle`
  (`crates/lash-core/src/runtime/process_work_driver.rs`).
- **Restate tier**: `RestateProcessIngressRunner` submits each process as a
  `LashProcessWorkflow/{process_id}/run` invocation via the Restate ingress;
  `RestateProcessDeployment` bundles the wiring
  (`crates/lash-restate/src/lib.rs`).

### 1.2 What is already push-based (do not rebuild)

`LashProcessWorkflow` (in `crates/lash-restate/src/lib.rs`) already has a
`#[shared] await_terminal` handler: `run` resolves a durable promise
(`restate_process_terminal_await_key`) on completion; `await_terminal` awaits
that promise. In-turn awaits already route through the effect layer:
`ProcessService::await_process` → `ProcessCommandRunner::run(ProcessCommand::Await)`
(`crates/lash-core/src/runtime/session_manager/process_runners/control.rs`) →
the deployment's effect controller. The **Restate** controller executes
`ProcessCommand::Await` as a workflow-to-workflow attach
(`RestateControllerContext::await_process_terminal`) — durable, zero polling.

### 1.3 The actual leak (what this plan fixes)

Everything that is *not* inside a Restate handler falls down to
`registry.await_process` / `registry.wait_event_after`, i.e. a store poll loop:

| Callsite | File | What it does today |
| --- | --- | --- |
| Inline effect controller, `ProcessCommand::Await` arm | `crates/lash-core/src/runtime/effect/executor.rs` (~line 1366) | `registry.await_process` |
| Sqlite store-backed effect controller, duplicated `execute_process_command` match | `crates/lash-sqlite-store/src/effect_replay.rs` (~line 602) | `registry.await_process` |
| Postgres store-backed effect controller, duplicated match | `crates/lash-postgres-store/src/postgres/effect_replay.rs` (~line 550) | `registry.await_process` |
| Facade admin `Processes::await_output` | `crates/lash/src/admin.rs` (~line 189) | `registry.await_process` directly — this backs downstream control planes' HTTP await endpoints |
| Worker cancel watcher | `crates/lash-core/src/runtime/process_worker.rs` (~line 476) | `registry.wait_event_after(process_id, "process.cancel_requested", 0)` |
| Tool event client (shell stdin streaming) | `crates/lash-core/src/tool_provider/process_events.rs` (`ToolProcessEventClient::wait_event_after`, ~line 83) | `registry.wait_event_after` |
| Core testing harness | `crates/lash-core/src/testing/mod.rs` (~line 612) | `registry.await_process` inside a test `ProcessService` impl |
| Facade testing harness | `crates/lash/src/testing.rs` (~line 386) | `registry.await_process` |
| Worker test helper | `crates/lash-core/src/runtime/process_worker.rs` (~line 782, `#[cfg(test)]`) | `registry.await_process` |
| Conformance suite | `crates/lash-core/src/testing/conformance/process_registry.rs` (~lines 547, 1055) | exercises both trait methods per store |
| Store unit tests | e.g. `crates/lash-sqlite-store/src/lib.rs` (~line 694), `crates/lash-restate/src/tests.rs` (several `.await_process(` calls) | trait methods |

Store-side implementations to delete:

- `crates/lash-sqlite-store/src/process_registry.rs`: `wait_event_after`
  (~968), `await_process` (~990), the `notify: Notify` field and every
  `notify.notify_waiters()` call in the write methods.
- `crates/lash-postgres-store/src/postgres/process_registry.rs`: same trio
  (`wait_event_after` ~521, `await_process` ~543, `notify` plumbing).
- `crates/lash-core/src/runtime/process/testing.rs`
  (`TestLocalProcessRegistry`): `wait_event_after` (~470), `await_process`
  (~498), the per-record `notify: Arc<Notify>` field and its
  `notify_waiters()` calls.

Note the pre-existing bug you are also fixing by deletion: the
`Notify::notified()`-inside-`select!` pattern in the sqlite/postgres loops has
a lost-wakeup window (a notification landing between the read and the select
entry is missed, costing a full poll interval). The replacement uses a
versioned `tokio::sync::watch` channel, which cannot lose updates.

### 1.4 Decisions already made (do not relitigate)

- **No store-side watch capability.** No `WorkItemWatch`, no LIVE
  SELECT/LISTEN-NOTIFY plumbing behind the persistence traits. Stores are pure
  state. (Project memory: `lash-work-item-wait-on-engine-seam`.)
- **`await_terminal` is the engine-native hot path** (this is what generated
  the 80k reads downstream). `await_event` keeps the core default everywhere —
  its only production consumer is same-process shell stdin streaming
  (`crates/lash-tools/src/shell/mod.rs` ~line 360,
  `SHELL_STDIN_SIGNAL_EVENT`), and the corrected architecture explicitly
  blesses "in-process notify + store-poll-with-backoff as the default impl of
  the engine seam's wait capability". Do not invent a Restate promise scheme
  for arbitrary event waits in this change.
- **No deadline parameters** on the new APIs. Callers bound waits with
  `tokio::time::timeout`, consistent with `Session::await_queued_work_batch`'s
  documented contract.
- **`Session::await_queued_work_batch`** (`crates/lash/src/session.rs` ~742)
  is out of scope: different store segment (queued-work batches, not
  processes), already backs off 25→400ms.

---

## 2. End-state architecture

### 2.1 Invariants

1. `ProcessRegistry` contains **no blocking methods**. Point reads and writes
   only.
2. All process waiting flows through **one** seam: `ProcessWorkDriver`
   (`await_terminal`, `await_event`), or — where no driver exists —
   the same `ProcessAwaiter` default constructed directly over the registry.
3. There is exactly **one** wait-loop implementation in the workspace:
   `ProcessAwaiter`. No store crate contains a sleep, notify, or loop for
   waiting.
4. Restate deployments never poll for terminal results: `ProcessAttach`
   implemented by `RestateProcessIngressRunner` attaches to the workflow's
   durable promise via a synchronous ingress call.
5. There is exactly **one** `ProcessCommand` execution match in the workspace
   (in lash-core), consumed by the inline controller and both store-backed
   controllers.

### 2.2 New components (all in `crates/lash-core/src/runtime/process/`, new file `awaiter.rs` unless noted)

```rust
/// In-process, coalescing change signal for process records, keyed by
/// process id. Version-counted via tokio::sync::watch so wakeups are
/// never lost. Signal-only: subscribers re-read the registry after a
/// bump; correctness never depends on payloads.
pub struct ProcessChangeHub { /* Arc<Mutex<HashMap<String, watch::Sender<u64>>>> */ }

impl ProcessChangeHub {
    pub fn new() -> Self;
    /// Subscribe before reading. Creates the entry if absent.
    pub fn subscribe(&self, process_id: &str) -> tokio::sync::watch::Receiver<u64>;
    /// Bump the version for `process_id`. Drops the map entry when no
    /// receivers remain (GC).
    pub fn notify(&self, process_id: &str);
}
```

```rust
/// Decorator that bumps a ProcessChangeHub after every mutating registry
/// call. The single choke point that gives same-process waiters push
/// latency regardless of which code path wrote.
struct WatchedProcessRegistry { inner: Arc<dyn ProcessRegistry>, hub: ProcessChangeHub }

/// Wrap a raw registry. Returns the decorated handle plus the hub.
/// Wiring calls this once per deployment, as early as possible; every
/// component must receive the *decorated* handle.
pub fn watch_process_registry(
    inner: Arc<dyn ProcessRegistry>,
) -> (Arc<dyn ProcessRegistry>, ProcessChangeHub);
```

`WatchedProcessRegistry` implements `ProcessRegistry` by delegation. Bump the
hub (`self.hub.notify(process_id)`) after these mutating methods succeed:
`register_process`, `set_external_ref`, `append_event`, `complete_process`,
`set_process_wait`, `clear_process_wait`, `mark_wake_enqueued`,
`discard_wake_delivery`, and `redrive_wake_delivery`. Pure reads and
lease/observer methods delegate without bumping (waits only observe record
status and the event log; observer/lease traffic would just cause
spurious re-reads). `durability_tier` delegates.

```rust
/// The single wait-loop implementation. Default ("engineless") waiting:
/// in-process hub push when available, exponential-backoff point reads
/// as the cross-process floor.
#[derive(Clone)]
pub struct ProcessAwaiter {
    registry: Arc<dyn ProcessRegistry>,
    hub: Option<ProcessChangeHub>,
}

impl ProcessAwaiter {
    pub fn new(registry: Arc<dyn ProcessRegistry>, hub: ProcessChangeHub) -> Self;
    /// No hub: pure backoff polling. Used only where wiring has no hub
    /// (e.g. a bare registry handed to the facade without a driver).
    pub fn polling(registry: Arc<dyn ProcessRegistry>) -> Self;

    /// Resolve when the process reaches a terminal state. Unknown id => Err
    /// (same contract as the deleted registry method).
    pub async fn await_terminal(&self, process_id: &str)
        -> Result<ProcessAwaitOutput, PluginError>;

    /// Resolve with the first event of `event_type` with
    /// sequence > after_sequence, including events that already exist
    /// (history is checked before waiting — same contract as the deleted
    /// registry method).
    pub async fn await_event(&self, process_id: &str, event_type: &str, after_sequence: u64)
        -> Result<ProcessEvent, PluginError>;
}
```

**Loop shape (correctness-critical, same for both methods):**

```text
let mut rx = hub.subscribe(process_id)   // SUBSCRIBE BEFORE FIRST READ
let mut backoff = 25ms
loop {
    check condition via point read (get_process / events_after)
    if satisfied -> return
    select! {
        _ = rx.changed()      => { backoff = 25ms }   // push: re-check now
        _ = sleep(backoff)    => { backoff = min(backoff * 2, 1s) }
    }
}
```

`watch::Receiver::changed()` is versioned: a bump landing between the read and
the `select!` is seen immediately. Both methods are cancel-safe (no state held
across awaits). With no hub, the loop is the `sleep` arm only.

```rust
/// Engine-native terminal attach. Implemented by engines whose substrate
/// can hold a wait natively (Restate ingress attach). Deliberately
/// terminal-only: event waits stay on the ProcessAwaiter default.
#[async_trait::async_trait]
pub trait ProcessAttach: Send + Sync {
    async fn await_terminal(&self, process_id: &str)
        -> Result<ProcessAwaitOutput, PluginError>;
}
```

### 2.3 `ProcessWorkDriver` becomes the await surface

In `crates/lash-core/src/runtime/process_work_driver.rs`:

```rust
#[derive(Clone)]
pub struct ProcessWorkDriver {
    registry: Arc<dyn ProcessRegistry>,      // decorated
    run_handle: Arc<dyn ProcessRunHandle>,
    awaiter: ProcessAwaiter,
    attach: Option<Arc<dyn ProcessAttach>>,
    hub: ProcessChangeHub,
}

impl ProcessWorkDriver {
    /// Wraps `registry` with watch_process_registry internally. All
    /// existing constructors funnel here.
    pub fn new(registry: Arc<dyn ProcessRegistry>, run_handle: Arc<dyn ProcessRunHandle>) -> Self;
    pub fn with_attach(mut self, attach: Arc<dyn ProcessAttach>) -> Self;
    /// Construct from an already-watched pair (used when the deployment
    /// wrapped early so other components share the same decorated handle).
    pub fn from_watched(
        registry: Arc<dyn ProcessRegistry>, hub: ProcessChangeHub,
        run_handle: Arc<dyn ProcessRunHandle>,
    ) -> Self;

    pub fn process_registry(&self) -> Arc<dyn ProcessRegistry>;   // decorated
    pub fn change_hub(&self) -> ProcessChangeHub;
    pub fn awaiter(&self) -> ProcessAwaiter;

    /// Await a work item's terminal output. Engine-native attach when the
    /// deployment provides one; the core awaiter otherwise. No fallback
    /// from a failed attach — errors surface.
    pub async fn await_terminal(&self, process_id: &str)
        -> Result<ProcessAwaitOutput, PluginError>;
    pub async fn await_event(&self, process_id: &str, event_type: &str, after_sequence: u64)
        -> Result<ProcessEvent, PluginError>;
}
```

`await_terminal` semantics: first do a `registry.get_process` point read and
return immediately when the record is already terminal (this also covers the
attach-before-submit race cheaply); otherwise `attach` when present, else
`awaiter`.

`ProcessRunHandle` is unchanged (submit-only). `InlineProcessRunHandle` is
unchanged. `ProcessWorkDriver::inline(...)` keeps working, funneling through
`new`.

### 2.4 Restate-native attach (in `crates/lash-restate/src/lib.rs`)

1. `RestateIngressClient` gains the synchronous sibling of
   `send_workflow_json`:

   ```rust
   pub async fn call_workflow_json<T: Serialize + ?Sized, R: DeserializeOwned>(
       &self, workflow: &str, workflow_key: &str, handler: &str, body: &T,
   ) -> Result<R, RestateHttpError>
   ```

   POST `{ingress_url}/{workflow}/{workflow_key}/{handler}` (no `/send`),
   JSON body, parse the 2xx response body as `R`; non-2xx becomes a
   `RestateHttpError` variant carrying status + body (add one if none fits).
   `reqwest::Client::new()` has no default total timeout, so long-held attach
   calls are safe with the existing client; do not add a timeout here.

2. `RestateProcessIngressRunner` implements `ProcessAttach`:

   ```rust
   #[async_trait::async_trait]
   impl ProcessAttach for RestateProcessIngressRunner {
       async fn await_terminal(&self, process_id: &str) -> Result<ProcessAwaitOutput, PluginError> {
           self.ingress
               .call_workflow_json::<_, ProcessAwaitOutput>(
                   "LashProcessWorkflow", process_id, "await_terminal",
                   &RestateProcessAwaitRequest { process_id: process_id.to_string() },
               )
               .await
               .map_err(/* into PluginError via RestateEffectError */)
       }
   }
   ```

   Semantics match the shipped workflow-to-workflow attach
   (`RestateControllerContext::await_process_terminal`): the shared handler
   awaits the durable promise; attaching before `run` starts simply pends on
   the promise. The driver-level terminal short-circuit (2.3) covers records
   completed before submission.

3. `RestateProcessDeployment::new` restructures to wrap early:
   `let (registry, hub) = watch_process_registry(raw)`; build the ingress
   runner over the decorated registry; build the driver with
   `ProcessWorkDriver::from_watched(registry, hub, Arc::new(runner.clone()-or-Arc))
   .with_attach(runner)` — one `Arc<RestateProcessIngressRunner>` used as both
   `ProcessRunHandle` and `ProcessAttach`.

### 2.5 One `ProcessCommand` executor

Create `ProcessLocalExecution::execute(command: ProcessCommand)` in lash-core
(natural home: next to the existing inline match in
`crates/lash-core/src/runtime/effect/executor.rs`, as a method on the
`ProcessLocalExecution` struct that already holds
`{ registry, process_work_driver: Option<ProcessWorkDriver> }`).

- Move the inline controller's match body into it. The `Await` arm becomes:
  driver present → `driver.await_terminal(&process_id)`; driver absent →
  `ProcessAwaiter::polling(registry).await_terminal(...)` (still loop-in-core,
  never a store call).
- Diff the three existing matches (inline executor, sqlite
  `effect_replay.rs::execute_process_command`, postgres equivalent) arm by
  arm. They are expected to be semantically identical except that the inline
  `Start` arm drives `process_work_driver.claim_and_run_pending` — the unified
  helper keeps that conditional on the driver being present, which preserves
  each caller's behavior. If you find a *real* semantic divergence, preserve it
  behind explicit code, not by keeping the duplicate match, and record it in
  the PR description.
- Delete both store crates' `execute_process_command` matches and have their
  `RuntimeEffectCommand::Process` arms call the shared helper via
  `local_executor.into_process()?.execute(*command)`.
- Update the `RuntimeEffectLocalExecutor::processes(...)` construction sites
  to pass the driver: `crates/lash-core/src/runtime/session_manager/process_runners/control.rs`
  (~line 208; `self.current.host.process_work_driver` is in scope),
  `crates/lash/src/core.rs` (~line 441; `self.env.process_work_driver`),
  `crates/lash-core/src/triggers.rs` (~line 973; thread the driver from the
  trigger context — it exists wherever a registry does in real wiring; if the
  trigger path genuinely has no driver handle, pass `None`, the awaiter
  fallback keeps it correct). Prefer collapsing `processes` /
  `processes_with_driver` into a single constructor taking
  `Option<ProcessWorkDriver>` — two constructors for one struct is the kind of
  residue this cutover removes.

### 2.6 Facade and worker rerouting

- `crates/lash/src/admin.rs` `Processes::await_output`: route through
  `self.core.env.process_work_driver` (`driver.await_terminal`). When the env
  has a registry but no driver (possible for exotic embedders), fall back to
  `ProcessAwaiter::polling(registry)`. Keep the public signature unchanged —
  downstream control planes get the fix from the version bump alone.
- Facade wiring (`crates/lash/src/core.rs`): for
  `ProcessWorkSource::Inline{registry}`, wrap once at build time
  (`watch_process_registry`), store the decorated handle in
  `env.process_registry`, and hand the watched pair into
  `ProcessWorkDriverSetup::LazyDefault` so the lazily-built driver shares the
  same hub (extend the `LazyDefault` config carrier with the hub). For
  `ProcessWorkSource::External(driver)`, `env.process_registry` already comes
  from `driver.process_registry()` (decorated by 2.3/2.4) — verify, don't
  duplicate the wrap. Mirror the same in `crates/lash/src/session.rs` (~line
  220) where `env.process_work_driver` is set.
- `DurableProcessWorker` cancel watcher
  (`crates/lash-core/src/runtime/process_worker.rs` ~476): replace
  `registry.wait_event_after(...)` with an awaiter call.
  `DurableProcessWorkerConfig` gains the hub: when constructed by facade
  wiring or `RestateProcessDeployment`, pass the shared hub
  (`.with_change_hub(hub)` or equivalent); the worker builds
  `ProcessAwaiter::new(registry, hub)` (or `polling` when never given one —
  only reachable from hand-rolled test wiring).
- `ToolProcessEventClient::wait_event_after`
  (`crates/lash-core/src/tool_provider/process_events.rs`): the
  `ToolProcessEventContext` struct (`crates/lash-core/src/tool_provider.rs`
  ~161) gains an `awaiter: ProcessAwaiter` field, threaded from the same
  wiring that currently threads `registry` into it; the wait call moves to the
  awaiter. `emit`/`emit_request` keep using the registry (writes).
- Testing harnesses: `crates/lash-core/src/testing/mod.rs` (~612) and
  `crates/lash/src/testing.rs` (~386) switch from `registry.await_process` to
  the driver/awaiter they already have access to (thread it if not).
- Worker test helper (`process_worker.rs` ~782 in `#[cfg(test)]`): use
  `ProcessAwaiter` over the test registry.

---

## 3. Execution order (compile-green checkpoints)

Work on a short-lived branch off `main`. Each step should build
(`cargo build --workspace`) before moving on.

**Step 1 — additive core.** Add `ProcessChangeHub`, `WatchedProcessRegistry` +
`watch_process_registry`, `ProcessAwaiter`, `ProcessAttach` in
`crates/lash-core/src/runtime/process/awaiter.rs`; export via
`runtime/process.rs` and `lib.rs` re-exports (match the existing export style
around `ProcessRegistry`/`ProcessWorkDriver`). Unit-test the hub and awaiter
in the same file (see §4.1).

**Step 2 — driver becomes the await surface.** Extend `ProcessWorkDriver` per
§2.3. Update `ProcessWorkDriver::inline` and all in-repo constructors.

**Step 3 — unify the ProcessCommand executor.** Per §2.5. This step touches
lash-core, lash-sqlite-store, lash-postgres-store, and the three
`RuntimeEffectLocalExecutor::processes` construction sites. The store crates'
`effect_replay.rs` files shrink by their duplicated match.

**Step 4 — Restate attach.** Per §2.4: `call_workflow_json`, `ProcessAttach`
impl, `RestateProcessDeployment` rewiring.

**Step 5 — reroute the remaining waiters.** Per §2.6 (facade admin, facade
wiring/env, worker cancel watcher, tool event client, testing harnesses).

**Step 6 — the deletion.** Remove `await_process` and `wait_event_after` from
the `ProcessRegistry` trait. Fix every compile error by deleting (never by
re-adding a loop):

- sqlite/postgres registries: both methods, the `notify` field, all
  `notify_waiters()` calls, the now-unused `Notify`/`Duration` imports.
- `TestLocalProcessRegistry`: both methods, per-record `notify` field and its
  bumps.
- Any remaining test callsites (`lash-restate/src/tests.rs`,
  `lash-sqlite-store/src/lib.rs` ~694, sim/scenario helpers the compiler
  surfaces): switch to `ProcessWorkDriver::await_terminal` or a
  `ProcessAwaiter` over the test registry, whichever the test already has
  wiring for.

**Step 7 — conformance and docs.** Per §4.2 and §5.

Run `cargo fmt --all` and `cargo clippy --workspace --all-targets` at the end;
rustfmt import-group ordering is enforced in this repo.

---

## 4. Test plan

### 4.1 New unit tests (lash-core, `awaiter.rs`)

- Hub: subscribe-then-notify wakes; notify-before-subscribe is not required to
  wake (documented semantics); entry GC after last receiver drops.
- Awaiter lost-wakeup regression: reader task checks a non-terminal record,
  writer completes + bumps *between* the read and the wait — awaiter must
  resolve on the next loop iteration (versioned watch), not after a backoff
  tick. Structure the test around a hub bump fired immediately after the
  first read; assert resolution well under the 1s cap.
- `await_event` returns a historical event immediately (no wait).
- `await_terminal` on unknown process id errors.
- `ProcessAwaiter::polling` (no hub) resolves via backoff.
- `WatchedProcessRegistry` bumps on each mutating method (spy hub / receiver
  version assertions).
- Driver: terminal short-circuit returns without invoking attach (use a panic
  attach stub); attach errors propagate (no silent poll fallback).

### 4.2 Conformance moves

`crates/lash-core/src/testing/conformance/process_registry.rs` currently
asserts wait behavior per store (~lines 547, 1055). Rework those cases to
exercise `ProcessAwaiter` (constructed via `watch_process_registry` over the
store under test) instead of registry methods:

- existing-event immediate return (`await_event`);
- cross-task completion resolves (`await_terminal` in a spawned task, complete
  from the test task, assert resolution — with the hub this must be prompt;
  keep the assertion generous, e.g. under 2s, to stay CI-safe);
- terminal output equality with what `complete_process` wrote.

Conformance keeps running against sqlite, postgres, and the in-memory
registry, so store coverage is preserved without stores owning wait code.
Check `testing/conformance/effect_host.rs` for `ProcessCommand::Await`
coverage and update construction to the unified executor if it constructs
`RuntimeEffectLocalExecutor::processes` directly.

### 4.3 Restate

`crates/lash-restate/src/tests.rs` has mock `RestateControllerContext` impls
with `await_process_terminal` — untouched by design (in-turn path). Add a
test for `call_workflow_json` against the existing HTTP-mock infrastructure
used by `send_workflow_json` tests (response body = handler JSON output), and
one for the `ProcessAttach` impl mapping errors to `PluginError`.

### 4.4 Commands

`cargo test -p lash-core -p lash-sqlite-store -p lash-postgres-store
-p lash-restate -p lash -p lash-tools`, then `cargo test --workspace` (or the
`justfile` test target if one exists — check `just -l`). Postgres-backed tests
follow whatever container/env convention the existing postgres store tests
use; do not invent a new one.

---

## 5. Docs, ADR, memory

- Add `docs/adr/` entry (follow the existing ADR format in that directory):
  "Process waits live on the work-driver seam" — store traits are pure state;
  coordination waits belong to the engine seam; Restate = ingress attach to
  the `LashProcessWorkflow` durable promise; default = core awaiter (hub +
  backoff). Reference the measured downstream cost as motivation.
- `grep -ri "await_process\|wait_event_after" docs/ CONTEXT.md PRODUCT.md` and
  update any prose describing waiting as a registry/store capability
  (`docs/persistence.html`, `docs/architecture*` are the likely hits; docs are
  hand-written HTML).
- Doc-comment the deleted-methods rationale on `ProcessRegistry` briefly
  ("waits live on `ProcessWorkDriver`; see ADR-NNN") so future impls don't
  re-add them.

---

## 6. Out of scope (do not touch)

- The `figments` repository (downstream plan: delete its two surreal poll
  loops after the version bump; verify its HTTP await endpoint long-holds).
- `Session::await_queued_work_batch` (queued-work segment, already backs off).
- The in-turn Restate await path (`RestateControllerContext::await_process_terminal`,
  `LashProcessWorkflow::await_terminal` handler) — already correct; you are
  adding an ingress-client consumer next to it, not changing it.
- `lash-remote-protocol` — no wait APIs live there.
- Any new deadline/timeout parameters on wait APIs.

---

## 7. Conventions and gotchas

- Commits: author-only attribution — **no** AI/Claude co-author trailers or
  mentions, ever. Never write the bracketed skip-ci token literally in a
  commit message (it suppresses CI); write "skip-ci" if you must refer to it.
- Full CI runs on pull requests and `main`; releases are dispatched manually
  from a green `main` commit.
  Run the full workspace test suite locally before declaring done.
- This is a pre-1.0 alpha workspace: the `ProcessRegistry` trait break is
  intended and release-noted by the pipeline; do not add deprecation shims.
- Double-wrapping `watch_process_registry` is harmless but wasteful; the
  wiring rules in §2.4/§2.6 exist so each deployment wraps exactly once.
  Assert in review that `env.process_registry` and
  `driver.process_registry()` are the same decorated instance in the facade
  inline path.
- Keep `ProcessAwaiter` reads as point reads: `get_process` for terminals;
  `events_after(process_id, after_sequence)` for events (do not widen to
  full-history scans).
- The backoff constants (25ms floor, 2x, 1s cap) are deliberate: interactive
  first-hit latency, ~1 read/sec steady-state worst case per waiter without a
  hub — versus 20–25/sec today.

## 8. Acceptance checklist

- [ ] `grep -rn "await_process\|wait_event_after" crates/*/src --include="*.rs"`
      shows no `ProcessRegistry` method definitions or calls; only
      `ProcessWorkDriver`/`ProcessAwaiter`/`ProcessService`/Restate-context
      symbols remain.
- [ ] `grep -rn "notify" crates/lash-sqlite-store/src/process_registry.rs
      crates/lash-postgres-store/src/postgres/process_registry.rs` is empty.
- [ ] Exactly one `ProcessCommand` match in the workspace (lash-core).
- [ ] `cargo test --workspace` green; `cargo clippy --workspace --all-targets`
      clean; `cargo fmt --all --check` clean.
- [ ] ADR added; stale doc prose updated.
- [ ] PR description lists any intentionally-preserved semantic differences
      found while unifying the three ProcessCommand matches (expected: none
      beyond the Start-arm driver drive).
