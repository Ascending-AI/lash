# Figments migration to lash 1.0

Every Figments site that has to change when Figments moves to lash 1.0's
durable substrate, as a checklist. The [host guide](durable-hosting.md)
defines the 1.0 surfaces each row moves to.

The sites were read from Figments `origin/main` at `8ac063f58a`. That tree
pins lash at `fa443796` (2026-08-25), which still ships `lash-restate`. The
checklist covers the durable substrate and host event ownership under
[ADR 0137](../adr/0137-the-host-owns-events-routing-and-scheduling.md). Other facade changes since that
pin appear only where they touch these sites.

Each row is marked:

- **mechanical:** the 1.0 replacement is fixed; the change is a rewrite;
- **contract change:** Figments' code has to take on a different contract,
  with the shape known;
- **decision needed:** Figments has to choose first. The choices are listed in
  [Decisions](#decisions).

## Committed cell call verdicts (FIG-6357)

Replace matches on `ExecutedCallOutcome::Err` with
`ExecutedCallOutcome::Failed { code, message }`, and construct or match success
as `ExecutedCallOutcome::Ok {}`. The verdict is `Clone` rather than `Copy`
because failure carries owned text. The stored and wire `outcome`
member changes in place from `"ok"` / `"err"` to `{"kind":"ok"}` /
`{"kind":"failed","code":"...","message":"..."}`. A cell's mirrored call entries now carry Lash's
settle failure, including an output-schema refusal of the host's Success;
render those details directly instead of issuing a read for each card.
The message is at most 4,096 bytes, cut on a UTF-8 boundary. Call
`DurableSession::read_tool_call` when the full message or typed cause is needed.
A cancelled call carries code `cancelled` and its cancellation message.
Update host mirror decoders with the pin; the pre-1.0 version freeze provides
no legacy decoder or migration between these development shapes.

## Provider file refusals (FIG-7265)

A provider refusal without structured file identity no longer invalidates
cached uploads or automatically re-uploads. In particular, an Anthropic
message-only missing-file refusal reaches the host as the ordinary typed
transport failure. Surface that failure through the existing provider error
path; no new configuration or public failure variant is required.

## Restate deployment and configuration

Figments also runs Restate for its own services (control-plane,
indexing-worker, docling-service). Those stay unless Figments decides
otherwise ([D1](#decisions)). The rows below are the lash parts only.

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `Cargo.toml:196` | Depends on `lash-restate` at the lash pin. | Remove it; the crate no longer exists. | mechanical |
| [ ] | `Cargo.toml:181` | Pins `lash` and its crates at `fa443796`. | Pin every lash crate at the 1.0 release. | mechanical |
| [ ] | `apps/lash-runtime/Cargo.toml:39` | `lash-restate.workspace = true`. | Remove. | mechanical |
| [ ] | `apps/lash-runtime/Cargo.toml:47` | `restate-sdk.workspace = true`, for lash's handlers and Figments' own objects. | Keep only if lash-runtime still hosts Figments-owned Restate objects (D1); lash needs no SDK. | decision needed |
| [ ] | `apps/lash-runtime/src/main.rs:134` | Imports `LashDurableWaitIndex`, `LashDurableWaitWorkflow`, `LashProcessWorkflow`, `RestateControllerContext`, `RestateEffectHost`, `RestateProcessDeployment`, `RestateRuntimeEffectController`, `RestateTurnDeployment`. | Delete the import; every type is gone. | mechanical |
| [ ] | `apps/lash-runtime/src/main.rs:448` | Builds `RestateIngressClient` from `RESTATE_INGRESS_URL` and, at `:452`, `RestateAdminClient` from `RESTATE_ADMIN_URL`, for lash and work items. | Lash needs neither. Keep them only for Figments' own objects (D1). | decision needed |
| [ ] | `apps/lash-runtime/src/main.rs:491` | Builds `RestateProcessDeployment` and `RestateTurnDeployment`, takes their effect host and work drivers, and at `:497` refuses to start unless the turn driver's tier is `Durable`. | Delete. Build one `Backend` with `lash::durable::DurableBackendBuilder` (row [B1](#backend-construction)); every 1.0 backend is durable, so the tier check goes. | mechanical |
| [ ] | `apps/lash-runtime/src/main.rs:587` | Builds a `DurableProcessWorker` and wraps it in `process_deployment.workflow(..)`. | Delete. Processes run as actors on the lash node that claims them. | mechanical |
| [ ] | `apps/lash-runtime/src/main.rs:595` | Binds `RESTATE_BIND_ADDR` (default `0.0.0.0:9082`) and serves `LashBootstrap`, `LashWorkItem`, `LashCronJob`, `LashProcessWorkflow`, `LashDurableWaitWorkflow` and `LashDurableWaitIndex` on a Restate endpoint until shutdown. | Serve a lash node instead (`node::serve`, [guide §1](durable-hosting.md#1-what-a-host-runs)), stopped on the same shutdown signal. `LashProcessWorkflow` and both wait services are deleted. `LashWorkItem`, `LashCronJob` and `LashBootstrap` are Figments' and follow D1. | contract change |
| [ ] | `apps/lash-runtime/src/durability.rs:22` | `bounded_retry_service_options`: Restate inactivity (15 min), abort (5 min) and retry options for every lash-runtime service. | Delete for lash work: lash bounds work with `ExecutionBudgets` and parks crash loops at `activation_loop_budget`. Keep for Figments' own objects only under D1. | decision needed |
| [ ] | `apps/lash-runtime/src/journal.rs:73` | `FallibleRestateContext`, a local `ctx.run` seam because the pinned controller journals `Result` as success. | Delete with the Restate controller. Nothing in lash 1.0 journals. | mechanical |
| [ ] | `apps/lash-runtime/src/bootstrap.rs:6` | `LashBootstrap` Restate service, submitted by deploy tooling after registration. | Its reconcile runs at node start or as a Figments task; no registration triggers it. | decision needed |
| [ ] | `apps/lash-runtime/src/config.rs:4` | `WORK_ITEM_SERVICE` and `CRON_JOB_SERVICE` Restate service names. | Delete, or keep for Figments-owned objects under D1. | decision needed |
| [ ] | `apps/lash-runtime/src/state.rs:20` | `AppState` holds `process_work_driver`, `turn_effect_host: Arc<RestateEffectHost>` and `turn_work_driver`, and at `:28` the Restate clients. | Hold one `lash::LashCore` (over one `Backend`). | mechanical |
| [ ] | `k8s/helm/figments/templates/lash-runtime.yaml:20` | Service port `restate`; container port at `:67`. | Remove: a lash node exposes no handler endpoint. | mechanical |
| [ ] | `k8s/helm/figments/templates/lash-runtime.yaml:87` | Env `RESTATE_BIND_ADDR`, `RESTATE_INGRESS_URL`, `RESTATE_ADMIN_URL`. | Remove for lash. There is no completion secret to add (row [K1](#completion-keys-and-resolve-webhooks)). | mechanical |
| [ ] | `k8s/helm/figments/values.yaml:595` | `lashRuntime.service.restatePort: 9082`. | Remove. | mechanical |
| [ ] | `k8s/helm/figments/templates/restate-registration.yaml:56` | Registers the lash-runtime endpoint and submits `LashBootstrap`; `:33` waits on the lash-runtime rollout. | Remove the lash-runtime entry, unless lash-runtime keeps Figments-owned objects (D1). | decision needed |
| [ ] | `k8s/helm/figments/files/restate-handlers.json:185` | Handler contracts for `LashWorkItem` (`:185`), `LashProcessWorkflow` (`:188`), `LashCronJob` (`:194`), `LashDurableWaitWorkflow` (`:203`), `LashDurableWaitIndex` (`:208`), `LashBootstrap` (`:215`). | Delete the lash-owned rows (`:188`, `:203`, `:208`); the Figments-owned rows follow D1. | decision needed |
| [ ] | `Tiltfile:716` | Registers `lash-runtime` on port `9082` with bootstrap `LashBootstrap`. | Remove, as in the Helm registration. | mechanical |
| [ ] | `tools/fleet/src/fleet/discovery.py:256` | Discovers the lash-runtime Restate endpoint at `:9082`. | Remove the lash-runtime endpoint. | mechanical |
| [ ] | `tools/fleet/src/fleet/retarget.py:452` | Retargets the lash-runtime Restate deployment on port `9082`. | Remove the lash-runtime row. | mechanical |
| [ ] | `apps/control-plane/crates/kernel/src/platform/config.rs:250` | Backlog gauge watches `LashDurableWaitWorkflow` and `LashDurableWaitIndex`. | Remove both. Watch lash's park feed and actor backlog instead ([guide §8](durable-hosting.md#parked-work-and-redrive)). | contract change |
| [ ] | `apps/control-plane/src/platform/bootstrap/startup.rs:296` | Always reports a paused series for `LashWorkItem`; the test at `:918` seeds the lash wait services. | Remove the lash wait services; `LashWorkItem` follows D1. | decision needed |
| [ ] | `scripts/test_restate_registration.py:114` | Checks every lash-runtime `#[restate_sdk::*]` trait against the handler contracts. | Drop `lash-runtime` from the list unless it keeps Restate objects (D1). | decision needed |

## Backend construction

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | <a id="backend-construction"></a>B1 `apps/lash-runtime/src/main.rs:403` | Connects `PostgresStorage` through `figments_lash_postgres::connect_postgres_storage` (`crates/figments-lash-postgres/src/storage_pool.rs:11`), then hands out each store separately (`session_store_factory`, attachments, artifacts, env store and process registry, `:460`–`:487`). | Build one `lash::postgres::PostgresStoreSet::new(&storage, attachments)` and one `Backend` with `DurableBackendBuilder::new(stores).config(..).build()`. The store ports come from the backend. | contract change |
| [ ] | `apps/lash-runtime/src/engine.rs:33` | `build_process_worker_core` builds a process `LashCore` with `.store_factory`, `.child_store_factory`, `.attachment_store`, `.process_env_store`, `.effect_host`, `.process_work_driver`, `.live_replay_store`. | `LashCore::builder(backend)`; the store, effect-host and work-driver setters are gone. | mechanical |
| [ ] | `apps/lash-runtime/src/turns.rs:712` | `run_agent_turn` builds a fresh `LashCore` per work item, with that item's provider, model, `max_turns` and plugins, over the shared Restate effect host. | One `LashCore` per node over one `Backend`. Per-turn provider, model and limits move into the session's `SessionSpec` and the send's `RunSpec`. Another node may run the turn, so nothing a turn needs may live only in the per-item core. | contract change |
| [ ] | `apps/lash-runtime/src/main.rs:208` | `RuntimeIdentity::from_env` (`LASH_RUNTIME_INSTANCE_ID`, else `HOSTNAME`, set to the pod name) and a fresh incarnation UUID at `:209`. | Serve the lash node as `NodeId` = the pod name. Each pod start is a new boot; two live pods must never share a name. | mechanical |
| [ ] | `apps/lash-runtime/src/main.rs:456` | `InMemoryLiveReplayStore` per pod; live streams are served by the pod that owns the turn. | A turn runs on whichever node claims its session, and moves on failover. Use one shared live replay store, `lash::postgres::PostgresLiveReplayStore`. | contract change |
| [ ] | `apps/lash-runtime/src/tests/turns.rs:30` | Tests build cores with `InlineEffectHost` and `InMemorySessionStoreFactory` (also `apps/lash-runtime/src/tests/triggers.rs:330` and `apps/lash-runtime/src/tests/runtime.rs:540`). | Both are retired. Build a backend over `lash::sqlite::SqliteStoreSet::memory()` with `DurableBackendBuilder`. | mechanical |

## Turn, work-item and process driving

Figments runs lash work inside its own Restate objects, with lash's Restate
controller as the effect context. On 1.0 the host submits and lash's engine
runs ([guide §1](durable-hosting.md#no-engine-server)).

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/work_items.rs:174` | `LashWorkItem`, a Restate virtual object, runs each work item in-process with `RestateRuntimeEffectController::new(ctx)` (`:203`). | Lash work is a `send()` on the session (agent turns) or a facade call (process start or cancel, and completion resolution). A Figments object may still sequence work items, but must not run a turn inside its handler (D1). | decision needed |
| [ ] | `apps/lash-runtime/src/work_items.rs:94` | Submits each work item to Restate (`send_json_object(WORK_ITEM_SERVICE, ..)`). | Under D1: submit to lash directly, or keep the Figments object. | decision needed |
| [ ] | `apps/lash-runtime/src/work_items/wait.rs:61` | Waits for a work item by polling the Restate invocation's output. | Wait on lash: the `SendHandle`, or `Processes::await_output` for a process. | contract change |
| [ ] | `apps/lash-runtime/src/turns.rs:571` | `run_agent_turn` takes a Restate controller and drives the turn in the work item's handler. | `session.send(input)` and follow its handle. The turn's provider calls, tool rounds and resume are lash's. | contract change |
| [ ] | `apps/lash-runtime/src/turns.rs:60`, `:88`, `:110`, `:136` | Process and event work items call the pinned facade under a Restate controller. | Start and cancel through the process facade. Move product event records and routing into Figments; after committing them, resolve a parked call, send with a stable turn id or start with a host key. Record each process binding before pruning can remove it (ADR 0137). | contract change |
| [ ] | `apps/lash-runtime/src/turns.rs:160` | `run_direct_llm_work_item` journals model resolution and the provider call in the Restate object. | Not lash work. It moves with the work-item decision (D1). | decision needed |
| [ ] | `apps/lash-runtime/src/providers.rs:35` | `resolve_model` journals the model-registry lookup with `run_fallible_json_send`. | For turns, resolve before `send()` and record the choice in the run spec; lash pins the model request before the first byte. | contract change |
| [ ] | `apps/lash-runtime/src/cron.rs:215` | `LashCronJob`, a Restate object that schedules itself with delayed sends (`:263`, `:278`, `:314`). | Keep it as a Figments scheduler that records each enabled registration's due tick, then starts or sends with a key derived from that tick (D1, ADR 0137). Figments owns the registration, timer and routing. | decision needed |
| [ ] | `apps/lash-runtime/src/cron/journaled.rs:114` | Emits the cron occurrence under `RestateRuntimeEffectController::new(ctx)` (also `apps/lash-runtime/src/cron/legacy.rs:80`). | Commit the tick in Figments storage, then deliver with a stable host start key or turn id. Reconcile unfinished delivery and retain its binding under ADR 0137. | mechanical |
| [ ] | `apps/lash-runtime/src/restate.rs:32` | Lists active `LashCronJob` invocations through the Restate admin API. | Follows the cron decision (D1). | decision needed |
| [ ] | `apps/lash-runtime/src/sessions.rs:552` | `request_turn_cancel_with_driver` cancels through `TurnWorkDriver::request_cancel` and reports its `durability_tier`. | Cancel through the facade's session cancel. A turn cancel is session mail and reaches the owning node. There is no tier. | mechanical |
| [ ] | `apps/lash-runtime/src/state.rs:306` | `TurnCancelMap` routes cooperative cancels to the replica running the turn (`:302`). | Delete: cancel is a durable request any node may write. | mechanical |
| [ ] | `apps/lash-runtime/src/state.rs:124` | `LiveSessionOwnerGuard` claims and renews a per-session owner lease (`claim_live_session_owner`, `crates/figments-lash-postgres/src/lib.rs:501`) for sticky routing, with `LASH_RUNTIME_SESSION_OWNER_TTL_SECS` (`:88`, default 90 s, `apps/lash-runtime/src/config.rs:12`). | Lash's session actor is the owner, fenced by its epoch, and moves between nodes. Delete the Figments lease and route by lash state; observation uses the shared live replay store. | decision needed |
| [ ] | `apps/lash-runtime/src/sessions.rs:40` | `observe_session` proxies to the owner pod (`StickyRoute::Proxy`, `:50`) or fails `no_active_session_owner`. | Serve from any pod over the shared live replay store. | contract change |

## Process engines

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/engine.rs:317` | `RuntimeAwareLashVmProcessEngine` implements `ProcessEngine` with `validate_start`, `run`, `identity` and `durability_tier`. `run` loads the process's env, rebuilds a `LashVmProcessEngine` with product-specific event declarations (`materialize`, `:282`) and runs it. | Delete the wrapper. The RLM protocol plugin factory registers lash's lash_vm engine, whose `advance` answers `Steps([vm_run])` ([guide §4](durable-hosting.md#4-host-process-engines)). The process log contains closed lifecycle facts only. Product events use host deferring tools; Figments selects each started process's tools and pins its definition explicitly (ADR 0137). | contract change |
| [ ] | `apps/lash-runtime/src/engine.rs:28` | `RuntimeAwareRlmProtocolPluginFactory` wraps the RLM factory to pass the artifact and env stores to the engine. | The stores come from the backend; keep the wrapper only for its surface contribution. | contract change |

This is the only `ProcessEngine` implementation in Figments.

## Live projections and exported descriptors

None. Figments never calls `Projections::export`, reads
`exported_descriptors`, or binds a live host object into a VM. Every host value
a VM sees is plain data, so the package Read cutover requires no projection-provider caller changes. Sites checked:

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/triggers/mod.rs:266` | Decodes a trigger source with the old VM's `HostDescriptor::decode`, which is plain data and has no kernel counterpart. | Keep the source descriptor as Figments data; registration, input mapping validation, provisioning and routing belong to the host (ADR 0137). | contract change |
| [ ] | `apps/control-plane/crates/chat/src/turn_runtime.rs:90` | Maps `ToolArgumentProjectionPolicy` to its remote form. | Keep the policy; define Figments' own transport projection around it. Lash supplies no wire DTO or conversion (ADR 0136). | mechanical |

Future host data reads are package Read tools registered through
`ToolProviderRegistration::with_reads` ([guide §7](durable-hosting.md#7-read-tools)).

## Completion keys and resolve webhooks

Figments mints and resolves no lash completion key today: lash-runtime calls
no `completions()` and no `resolve`. Its webhooks
(`apps/lash-runtime/src/main.rs:513`, Composio) remain Figments ingress. Figments commits their product event first, then
delivers through a keyed start, send or completion resolution under ADR 0137.

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | <a id="completion-keys-and-resolve-webhooks"></a>K1 `k8s/helm/figments/templates/lash-runtime.yaml:70` | Env from the chart's config map and secret; no completion secret. | Nothing to configure: lash keeps no completion secrets. A completion key is its wait's random 128-bit id and a bearer capability, so the callback endpoint authenticates and authorizes its caller before it resolves, and hands keys only to callers it has authorized ([guide §5](durable-hosting.md#5-completion-keys)). | mechanical |

When Figments adds a Pending tool or a process that waits on an external
system, its callback endpoint calls `Completions::resolve(key, resolution)`,
authenticates the caller first, retries until it gets an answer and treats
`AlreadyResolved` as success.

## Per-call timeouts that become `ExecutionBudgets`

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/durability.rs:7` | `ADMISSION_INACTIVITY_TIMEOUT` (15 min) and `ADMISSION_ABORT_TIMEOUT` (5 min, `:8`) bound every lash-runtime invocation, so the 840 s adaptive admission fits inside one. | Lash work is bounded by `ExecutionBudgets`: one model call by `model_total` (default 10 min) over throttle, backoff and attempts. Admission that waits up to 840 s for capacity must fit in `model_total`, or run before `send()`. | decision needed |
| [ ] | `apps/lash-runtime/src/work_items.rs:12` | `DEFAULT_BACKGROUND_ADMISSION_TIMEOUT_SECS = 840`, clamped below the Restate inactivity timeout (`:27`). | Derive the bound from `ExecutionBudgets::model_total` instead of a Restate timeout. | decision needed |
| [ ] | `k8s/helm/figments/values.yaml:549` | `backgroundAdmissionTimeoutSeconds: 840`, kept "below the 900s synchronous caller wait". | Configure `ExecutionBudgetsConfig` (`model_total`, `provider`) on `LashCoreBuilder::execution_budgets` from chart values. | decision needed |
| [ ] | `apps/lash-runtime/src/turns.rs:716` | `.max_turns(execution.max_turns.unwrap_or(12))` on the per-item core. | A turn budget on the run spec; execution time comes from `ExecutionBudgets`. | mechanical |
| [ ] | `apps/lash-runtime/src/tools.rs:854` | Tool-host HTTP calls run inline under Figments' own client timeouts. | Each tool's manifest declares a host-set `execution` bound and, if it can defer, an independent `park` bound (`Within(Duration)` or `UntilScopeEnd`). Lash defaults neither. Long work can use a process, isolated or Pending tool ([guide §6](durable-hosting.md#6-execution-budgets)). | contract change |

## SQLite

None to change. Figments uses no lash SQLite store: its cutover guard,
`scripts/ci/lash-runtime-cutover-guard.sh:33`, refuses `lash-sqlite-store` and
`rusqlite` in the workspace. Local and test runs that want lash without
PostgreSQL use one file through `SqliteStoreSet::open`, or
`SqliteStoreSet::memory()`; several processes on one machine may serve one
file, each as a node of its own
([guide §3](durable-hosting.md#3-sqlite-is-one-file)).

## PostgreSQL topology assumptions

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `k8s/helm/figments/values.yaml:102` | In-cluster `pgvector` PostgreSQL, one instance, no replica (`postgres.enabled: false` by default; production reads `LASH_DATABASE_URL` from its secret). | Choose the topology lash relies on: `Once` survives a failover only if acknowledged commits survive promotion. A single primary or a synchronous standby keeps it; an asynchronous replica does not ([guide §2](durable-hosting.md#durability-across-a-failover)). | decision needed |
| [ ] | `crates/figments-postgres/src/config.rs:139` | `<PREFIX>_TIMEOUT_POLICY=verify-defaults` prepares pools for a transaction-mode pooler (`crates/figments-lash-postgres/src/storage_pool.rs:39`). | Lash's listener needs `LISTEN` and a session advisory lock: connect it directly or in session mode. Through a transaction pooler only, set `Notifier::PollOnly` and accept lease-only crash detection. | decision needed |
| [ ] | `k8s/helm/figments/values.yaml:551` | Connection budget: lash-runtime holds 20 + 20 sqlx connections plus a 16-slot `PostgresStorage` pool per pod, already over-committed against `maxConnections: 400`. | Replace the storage pool with the `PostgresHostConfig` role pools and declare `deployment`, so the connect checks the rolling budget (see [`postgres.md`](postgres.md#figments-migration)). | contract change |
| [ ] | `apps/lash-runtime/src/main.rs:394` | Storage pool `acquire_timeout`, `idle_timeout`, `lock_timeout` and `statement_timeout` from Figments defaults. | Keep a statement timeout, and bound connects, so a hung heartbeat call ends ([guide §1](durable-hosting.md#clean-shutdown)). Set server TCP keepalives so a partitioned node's session ends. | mechanical |
| [ ] | `k8s/helm/figments/values.yaml:562` | KEDA scales lash-runtime 1–6 pods on in-flight LLM calls. | Scale-down must stop the node cleanly (complete `serve`'s stop future on SIGTERM) so its actors are released at once, not after a reap. Set the pod's termination grace above the stop time. | contract change |

## Process retention and projection

Process observations, terminals, lifecycle states and events encode only through
`lash::persistence::Durable(&record)` when relaying trusted records. The same
opt-in covers `ObservedProcessChange`, its `ChangePage` and `ProcessEventsRead`;
remove host-written serializers for those containers. Their existing decoders
and encoded shapes are unchanged.

`ToolValue`, `ToolCallOutcome`, `ToolCallOutput`, `ToolCallRecord`, `ToolFailure`,
`ToolCancellation`, `TurnControl`, `ToolControl`, `CachedToolSuccess` and `ProcessAwaitOutput` no
longer implement plain `Serialize`. Use `Durable` for persistence and trusted
transport. Workflow results and displayed JSON use `to_json_value()`,
`value_for_projection()` or the process `projection()`; model presentation uses
`ModelToolReturn::from_output()`. The compiler now refuses implicit tool-value
trust-envelope serialization as well as implicit process-record serialization.

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/main.rs:669` | `spawn_terminal_process_prune` calls `prune_terminal_process_scopes` on an interval. | That function is gone; the host schedules `core.processes().prune(cutoff_epoch_ms, filter, watermark)`. Supply the projector's acknowledged `ProjectionWatermark::UpTo` or explicitly `NoProjector`, retain rows through outstanding delivery/reconciliation and await horizons, and persist each start-key binding before pruning. Check `live_until_descendants` when the product needs subtree quiescence. | contract change |
| [ ] | `apps/control-plane/crates/workflows/src/projector.rs:62` | The workflow projector reads `process_changes` and treats a process terminal as the run's end, linking child processes (`:197`). | A terminal does not mean the subtree stopped. Decide whether a workflow run ends at its process terminal or when `live_until_descendants` is empty ([guide §8](durable-hosting.md#two-non-guarantees)). | decision needed |

### Maintenance ownership on the repin

The repin tree at Figments `d63c4a6ea32554b874fd751c30cbbfd2dbbbbd40`
already has host maintenance in `apps/lash-runtime/src/substrate.rs` and
terminal-process pruning in `apps/lash-runtime/src/main.rs`. Keep those host
jobs; serving a Lash node does not schedule them. Apply the
[maintenance contract](durable-hosting.md#host-scheduled-maintenance):

| Operation | Host action on the repin |
| --- | --- |
| Database attachment referrers | Retire retained records under host retention/deletion policy. Lash's transactions release their referrers; that cleanup does not physically delete attachment blobs. |
| Physical attachment bytes | Schedule `lash::persistence::reclaim_unreferenced_attachments` with the deployment's root authority and its exclusive blob namespace. Choose `AttachmentReclamationPolicy` grace and empty-root authority explicitly. The namespace can be an exclusive prefix in a shared bucket; it need not be an entire bucket. |
| Terminal process rows and events | Schedule `core.processes().prune` with the host's cutoff/filter and acknowledged projection watermark (or explicit `NoProjector`). Keep rows long enough for delivery reconciliation and still-replayable awaits; commit returned process bindings before prune can erase start-key deduplication. |

Figments' prefix validation at `substrate.rs:105`, sweep caller at `:189`
and empty-root refusal at `:223` are host choices. Preserve
`EmptyRootSetPolicy::Refuse` unless deletion of every unreferenced eligible
blob in the chosen namespace is intended; only then choose
`AuthorizeDeleteAll`. Grace covers terminal-owner intent retention and
recent blob writes, not a presumed maximum execution duration. Pair the
root authority with the correct namespace even when its roots are nonempty.

The current sweep adopts dead-pass condemnations and uses writer-intent and
delete-arming fences; it is not a snapshot-only delete. Inspect its `fence`
and `MaintenanceReport::sweep()` classification: `Ok` can still be
`Incomplete` with failed, deferred or stalled ids. Later scheduled sweeps
retry under backoff; `list_condemnations` supplies stalled reasons, attempt
counts and errors. A refusal or sweep-level failure carries a partial report.
Report those results operationally. The host also owns the process retention
floor: Figments' one-hour setting must cover its actual reconciliation and
projector lag, and does not by itself prove that horizon.

## Removed prompt text and placement

This inventory is recovered from Lash removal commit
`ed0b3e298ae67cbf4a31b9d09dcb34f267049906` (FIG-5432), comparing its parent
with its result. The historical sources are `crates/lash-core/src/protocol_copy.rs`,
`crates/lash-protocol-rlm/src/{prompt_sections.rs,driver.rs,dialect.rs}` and
`crates/lash-protocol-standard/src/prompt.rs`. These are migration evidence;
choose which presentation sections to retain in the host under
[ADR 0133](../adr/0133-prompt-sections-are-keyed-trusted-and-placed-by-the-host.md).
They do not belong back in the protocol prompt.

The literal strings below contain no leading/trailing spaces or newline.
`\n` in composition expressions means one LF byte; code fences show the
text, not an additional final newline.

| Removed section | Historical default placement and purpose |
| --- | --- |
| `rlm_protocol/intro` | `InitialInstructions`, first, before guidance and execution; `Turn` and `Compaction`. |
| `rlm_protocol/guidance` | `InitialInstructions`, after intro and before execution/declarations; `Turn` and `Compaction`. |
| `standard_protocol/intro` | `InitialInstructions`, before execution; `Turn` and `Compaction`. |
| `standard_protocol/guidance` | `InitialInstructions`, after execution; `Turn` and `Compaction`. |
| `rlm_protocol/final_answer_format` | `CurrentContext`, for `Turn` only, after `finalization` and any `required_output`, before any `context_budget`; omitted when no body is selected. |

Those are registration-order defaults. A host plan could move, order, wrap
or exclude them. The composer joined non-omitted sections in each placement
with `\n\n`. RLM placed the late composition after the projected iteration
and turn-cause prefix in a User message, separated from that prefix by
`\n\n\n`; it never appended these sections to stored conversation history.
Compaction offered no tools, so its guidance had only the three fixed bullets.

The removed intro, shared by both protocols, was exactly:

```text
You are an assistant operating the lash harness.
```

Guidance was `"## Guidance\n\n" + bullets.join("\n")`, with these
three fixed bullets in this order:

```text
- Be concise; no filler, hedging, or performative tone.
- Act as soon as the next step is clear; do not restate conclusions.
- Prefer the simplest correct solution.
```

When an `ask` tool was offered, the following fourth bullet was inserted
**between the first and second fixed bullets**, with the same single-LF
separators:

```text
- Take initiative when the user's intent is clear. Ask only when progress is blocked.
```

RLM tested the whole offered catalog, including non-inline tools. Standard
tested the visible catalog: all offered tools without discovery, only inline
tools when discovery was installed. This tool-name-selected interaction
policy was removed along with the copy; a host that retains it makes that
policy choice explicitly.

The answer-format section was exactly
`"=== FINAL ANSWER FORMAT ===\n\n" + body`: no leading LF and no trailing
LF. For the historical `Markdown` setting, `body` had these branches. The
shown `finish(value)` is the TypeScript dialect's then-current
`finish_statement`; the renderer interpolated the selected dialect's
spelling inside the backticks.

| Historical termination and schema | Selected body |
| --- | --- |
| `Natural`, no schema or a schema whose top-level `type` is exactly `"string"` | A below |
| `Natural`, any other schema | B below |
| `FinishRequired`, no schema | C below |
| `FinishRequired`, any schema | Omit the whole section, including its heading, for every format setting. |

A:

```text
Write prose-only final answers as nicely formatted Markdown. If you intentionally use `finish(value)`, use a Markdown string for user-facing answers, not a raw record/list/tool-result value.
```

B:

```text
Write prose-only final answers as nicely formatted Markdown.
```

C:

```text
When finishing, call `finish(value)` with a nicely formatted Markdown string, not a raw record/list/tool-result value.
```

Creation defaulted the historical format to `Markdown`. The renderer used
the run's resolved recorded options. An absent format or `RawFinalValue`
omitted the whole section. `Custom { guidance }` used `guidance.trim()` as
the body, preserving its internal whitespace, and omitted an empty result;
the `FinishRequired`-with-schema omission took precedence over Custom too.

Comparing Figments' `crates/figments-lash-runtime-api/src/prompt_text.rs`
at `d63c4a6ea32554b874fd751c30cbbfd2dbbbbd40` with those historical Lash
sources: `INTRO` (`:7`), `GUIDANCE_BULLETS` (`:11`),
`ANSWER_FORMAT_HEADING` (`:24`) and the three answer-format strings (`:27`
onward) match the removed TypeScript literals byte for byte. The host's
three-bullet array omits the conditional `ask` bullet; its
`AnswerTermination` mapping covers the Markdown branches, not the historical
Custom/Raw choices. Figments' `FigmentsPrompt::answer_format_text` wraps the
selected nonempty trimmed body with the same heading and `\n\n`. Its keyed
sections in `apps/lash-runtime/src/figments_prompt.rs` place intro/guidance
in initial instructions and answer format in current context; its additional
context section and appended instruction paragraphs are host text, not
removed protocol literals. The copy's comment cites Lash `38c71e0831`;
the inventory here uses the actual removal and its parent as authority.

### Completion after the removal

Main later changed completion in
`52e3c76f7b` (FIG-5823): finish is an **offered control tool**, whose
`FinishDeclaration::value_schema` types the value for that tool. The
protocol's finalization section names the finish tools actually offered;
their input contracts are in the tool catalog. There is no session-wide
`finish_schema`, separate `required_output` section or protocol
answer-presentation setting. Termination (`Natural` or `FinishRequired`)
is fixed at session creation; run options do not change it. A run needing
a different final value shape offers another finish tool through its tool
access. A host prompt renderer can inspect
`PromptInput::session().rlm_config()` and the offered manifests' finish
controls; it must base its guidance on those current contracts rather than
reusing the historical schema branches as a runtime API.

## Runbooks

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `runbooks/restate-registration/runbook.md:53` | Requires an accepted `LashBootstrap/run` after the lash-runtime endpoint registers. | Drop the lash-runtime step; lash-runtime does not register. | mechanical |
| [ ] | `runbooks/restate-cron-replay/runbook.md:13` | Replays a `LashCronJob` timer replacement across an SDK process kill. | Follows the cron decision (D1). | decision needed |
| [ ] | `runbooks/restate-invocation-output/runbook.md:43` | Proves the work-item completion wait over Restate invocation output. | Replace with a wait on the lash handle, or delete with the work-item object (D1). | decision needed |
| [ ] | `runbooks/work-item-failure-cache/runbook.md:66` | Inspects pending invocations of the removed `LashWorkItemWorkflow` service. | Delete the Restate step; failed lash work is in the park feed and turn outcomes. | mechanical |
| [ ] | `runbooks/llm-admission-lanes/runbook.md:26` | Requires exactly one lash-runtime pod, because admission state is per pod. | With several lash nodes, a turn's model call runs on the node that owns its session. Confirm the single-pod setup still pins every call. | contract change |

Figments' own ADRs that describe lash on Restate need superseding notes:
`docs/adr/0001-lash-runtime-cutover.md:30` (lash process work through
`LashProcessWorkflow`), `docs/adr/0045-durable-lane-failure-doctrine.md:76`
(the lash wait services) and `docs/adr/0121-fleet-treats-deployments-as-a-cache.md:96`
(`LashDurableWaitIndex` state).

## Decisions

- **D1. Figments-owned Restate objects that run lash work.** `LashWorkItem`,
  `LashCronJob` and `LashBootstrap` live in lash-runtime and drive lash inside
  their handlers. Either keep them as Figments objects that only submit to
  lash (`send()`, facade process calls and completion resolution) and wait on lash's
  handles, or delete them and submit from the HTTP routes, with cron as a
  Figments scheduler that commits each tick and then delivers through a keyed
  start or send under ADR 0137. Figments owns source registrations and routing. Keeping them keeps a second durable log in
  front of lash.
- **D2. PostgreSQL topology** for the lash database: synchronous standby, a
  single primary without failover, or an asynchronous replica with the
  weakened guarantee written down.
- **D3. Pooler mode** for the lash listener connection: direct or session
  mode, or `Notifier::PollOnly`.
- **D4. Adaptive admission against `model_total`:** raise `model_total` to
  cover the 840 s admission budget, or admit before `send()`.
- **D5. Session ownership and observation:** delete the Figments owner lease
  and sticky routing in favour of lash's actor ownership and a shared live
  replay store.
- **D6. Workflow run completion:** the process terminal, or the empty
  `live_until_descendants` subtree.
