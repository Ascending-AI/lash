# Figments migration to lash 1.0

Every Figments site that has to change when Figments moves to lash 1.0's
durable substrate, as a checklist. The [host guide](durable-hosting.md)
defines the 1.0 surfaces each row moves to.

The sites were read from Figments `origin/main` at `8ac063f58a`. That tree
pins lash at `fa443796` (2026-08-25), which still ships `lash-restate`. The
checklist covers the durable substrate only. Other facade changes since that
pin appear only where they touch these sites.

Each row is marked:

- **mechanical:** the 1.0 replacement is fixed; the change is a rewrite;
- **contract change:** Figments' code has to take on a different contract,
  with the shape known;
- **decision needed:** Figments has to choose first. The choices are listed in
  [Decisions](#decisions).

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
| [ ] | `k8s/helm/figments/templates/lash-runtime.yaml:87` | Env `RESTATE_BIND_ADDR`, `RESTATE_INGRESS_URL`, `RESTATE_ADMIN_URL`. | Remove for lash. Add the completion secret (row [K1](#completion-keys-and-resolve-webhooks)). | mechanical |
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
| [ ] | <a id="backend-construction"></a>B1 `apps/lash-runtime/src/main.rs:403` | Connects `PostgresStorage` through `figments_lash_postgres::connect_postgres_storage` (`crates/figments-lash-postgres/src/storage_pool.rs:11`), then hands out each store separately (`session_store_factory`, attachments, artifacts, env store, process registry, trigger store, `:460`–`:487`). | Build one `lash::postgres::PostgresStoreSet::new(&storage, attachments)` and one `Backend` with `DurableBackendBuilder::new(stores).config(..).completion_secrets(..).build()`. The store ports come from the backend. | contract change |
| [ ] | `apps/lash-runtime/src/engine.rs:33` | `build_process_worker_core` builds a process `LashCore` with `.store_factory`, `.child_store_factory`, `.attachment_store`, `.process_env_store`, `.effect_host`, `.process_work_driver`, `.trigger_store` and `.live_replay_store`. | `LashCore::builder(backend)`; the store, effect-host and work-driver setters are gone. | mechanical |
| [ ] | `apps/lash-runtime/src/turns.rs:712` | `run_agent_turn` builds a fresh `LashCore` per work item, with that item's provider, model, `max_turns` and plugins, over the shared Restate effect host. | One `LashCore` per node over one `Backend`. Per-turn provider, model and limits move into the session's `SessionSpec` and the send's `RunSpec`. Another node may run the turn, so nothing a turn needs may live only in the per-item core. | contract change |
| [ ] | `apps/lash-runtime/src/main.rs:208` | `RuntimeIdentity::from_env` (`LASH_RUNTIME_INSTANCE_ID`, else `HOSTNAME`, set to the pod name) and a fresh incarnation UUID at `:209`. | Serve the lash node as `NodeId` = the pod name. Each pod start is a new boot; two live pods must never share a name. | mechanical |
| [ ] | `apps/lash-runtime/src/main.rs:456` | `InMemoryLiveReplayStore` per pod; live streams are served by the pod that owns the turn. | A turn runs on whichever node claims its session, and moves on failover. Use one shared live replay store, `lash::postgres::PostgresLiveReplayStore`. | contract change |
| [ ] | `apps/lash-runtime/src/tests/turns.rs:30` | Tests build cores with `InlineEffectHost` and `InMemorySessionStoreFactory` (also `apps/lash-runtime/src/tests/triggers.rs:330` and `apps/lash-runtime/src/tests/runtime.rs:540`). | Both are retired. Build a backend over `lash::sqlite::SqliteStoreSet::memory()` with `DurableBackendBuilder`. | mechanical |

## Turn, work-item and process driving

Figments runs lash work inside its own Restate objects, with lash's Restate
controller as the effect context. On 1.0 the host submits and lash's engine
runs ([guide §1](durable-hosting.md#no-restate-deployment)).

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/work_items.rs:174` | `LashWorkItem`, a Restate virtual object, runs each work item in-process with `RestateRuntimeEffectController::new(ctx)` (`:203`). | Lash work is a `send()` on the session (agent turns) or a facade call (process start, cancel, signal, trigger emit). A Figments object may still sequence work items, but must not run a turn inside its handler (D1). | decision needed |
| [ ] | `apps/lash-runtime/src/work_items.rs:94` | Submits each work item to Restate (`send_json_object(WORK_ITEM_SERVICE, ..)`). | Under D1: submit to lash directly, or keep the Figments object. | decision needed |
| [ ] | `apps/lash-runtime/src/work_items/wait.rs:61` | Waits for a work item by polling the Restate invocation's output. | Wait on lash: the `SendHandle`, or `Processes::await_output` for a process. | contract change |
| [ ] | `apps/lash-runtime/src/turns.rs:571` | `run_agent_turn` takes a Restate controller and drives the turn in the work item's handler. | `session.send(input)` and follow its handle. The turn's provider calls, tool rounds and resume are lash's. | contract change |
| [ ] | `apps/lash-runtime/src/turns.rs:60` | `run_process_start_work_item`, `run_process_cancel_work_item` (`:88`), `run_process_signal_work_item` (`:110`) and `run_trigger_emit_work_item` (`:136`) call `processes()` and `triggers()` under `controller.scoped_effect_controller(..)`. | Call the same facade methods with the core's `ActorContext` (`LashCore::effect_host()`). Start, signal and trigger writes commit with their mailbox wakes. | mechanical |
| [ ] | `apps/lash-runtime/src/turns.rs:160` | `run_direct_llm_work_item` journals model resolution and the provider call in the Restate object. | Not lash work. It moves with the work-item decision (D1). | decision needed |
| [ ] | `apps/lash-runtime/src/providers.rs:35` | `resolve_model` journals the model-registry lookup with `run_fallible_json_send`. | For turns, resolve before `send()` and record the choice in the run spec; lash pins the model request before the first byte. | contract change |
| [ ] | `apps/lash-runtime/src/cron.rs:215` | `LashCronJob`, a Restate object that schedules itself with delayed sends (`:263`, `:278`, `:314`). | Keep it as a Figments object that calls `triggers().emit` (D1), or make each cron a lash process that sleeps (`EngineAction::Sleep`) and emits. | decision needed |
| [ ] | `apps/lash-runtime/src/cron/journaled.rs:114` | Emits the cron occurrence under `RestateRuntimeEffectController::new(ctx)` (also `apps/lash-runtime/src/cron/legacy.rs:80`). | `triggers().emit(request, core.effect_host())`; the occurrence's idempotency key stays the dedupe. | mechanical |
| [ ] | `apps/lash-runtime/src/restate.rs:32` | Lists active `LashCronJob` invocations through the Restate admin API. | Follows the cron decision (D1). | decision needed |
| [ ] | `apps/lash-runtime/src/sessions.rs:552` | `request_turn_cancel_with_driver` cancels through `TurnWorkDriver::request_cancel` and reports its `durability_tier`. | Cancel through the facade's session cancel. A turn cancel is session mail and reaches the owning node. There is no tier. | mechanical |
| [ ] | `apps/lash-runtime/src/state.rs:306` | `TurnCancelMap` routes cooperative cancels to the replica running the turn (`:302`). | Delete: cancel is a durable request any node may write. | mechanical |
| [ ] | `apps/lash-runtime/src/state.rs:124` | `LiveSessionOwnerGuard` claims and renews a per-session owner lease (`claim_live_session_owner`, `crates/figments-lash-postgres/src/lib.rs:501`) for sticky routing, with `LASH_RUNTIME_SESSION_OWNER_TTL_SECS` (`:88`, default 90 s, `apps/lash-runtime/src/config.rs:12`). | Lash's session actor is the owner, fenced by its epoch, and moves between nodes. Delete the Figments lease and route by lash state; observation uses the shared live replay store. | decision needed |
| [ ] | `apps/lash-runtime/src/sessions.rs:40` | `observe_session` proxies to the owner pod (`StickyRoute::Proxy`, `:50`) or fails `no_active_session_owner`. | Serve from any pod over the shared live replay store. | contract change |

## Process engines

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/engine.rs:317` | `RuntimeAwareLashlangProcessEngine` implements `ProcessEngine` with `validate_start`, `run`, `identity` and `durability_tier`. `run` loads the process's env, rebuilds a `LashlangProcessEngine` with that env's trigger event types (`materialize`, `:282`) and runs it. | Delete the wrapper. The RLM protocol plugin factory registers lash's lashlang engine, whose `advance` answers `Steps([vm_run])` ([guide §4](durable-hosting.md#4-host-process-engines)). The per-process trigger event types must reach the surface the factory's `LashlangRunSettingsRecorder` records at creation: contribute them as a plugin extension the recorder reads. | contract change |
| [ ] | `apps/lash-runtime/src/engine.rs:28` | `RuntimeAwareRlmProtocolPluginFactory` wraps the RLM factory to pass the artifact and env stores to the engine. | The stores come from the backend; keep the wrapper only for its surface contribution. | contract change |

This is the only `ProcessEngine` implementation in Figments.

## Live projections and exported descriptors

None. Figments never calls `Projections::export`, reads
`exported_descriptors`, or binds a live host object into a VM. Every host value
a VM sees is plain data, so decision 7 ("every live host object maps to a
`ResourceRef` plus a provider") has no gap in Figments. Sites checked:

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/triggers/mod.rs:266` | Decodes a trigger source with `lashlang::HostDescriptor::decode`, which is plain data. | No change. | mechanical |
| [ ] | `apps/control-plane/crates/chat/src/turn_runtime.rs:90` | Maps `ToolArgumentProjectionPolicy` to its remote form. | No change; the policy still exists. | mechanical |

A future Figments projection is a `ProjectionProvider` registered on the
builder, read by `ResourceRef` ([guide §7](durable-hosting.md#7-projection-providers)).

## Completion keys and resolve webhooks

Figments mints and resolves no lash completion key today: lash-runtime calls
no `completions()` and no `resolve`. Its webhooks
(`apps/lash-runtime/src/main.rs:513`, Composio) are trigger ingress, which
does not change.

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | <a id="completion-keys-and-resolve-webhooks"></a>K1 `k8s/helm/figments/templates/lash-runtime.yaml:70` | Env from the chart's config map and secret; no completion secret. | Provision a completion secret of at least 32 bytes, the same on every lash-runtime pod, and pass it to `CompletionKeySecrets::new` with a key version. A backend built without it is refused. Plan rotation as a second version ([guide §5](durable-hosting.md#5-completion-keys)). | decision needed |
| [ ] | `apps/lash-runtime/src/main.rs:363` | Reads `LASH_DATABASE_URL` and the pool settings; nothing reads a completion secret. | Read the secret beside the database URL and pass it to the builder. | mechanical |

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
| [ ] | `apps/lash-runtime/src/tools.rs:854` | Tool-host HTTP calls run inline under Figments' own client timeouts. | Each tool's manifest declares its `expected_execution`; an inline tool above `tool_ceiling` (default 5 min) is refused at registration. A longer tool becomes a process, isolated or Pending tool ([guide §6](durable-hosting.md#6-execution-budgets)). | contract change |

## SQLite

None to change. Figments uses no lash SQLite store: its cutover guard,
`scripts/ci/lash-runtime-cutover-guard.sh:33`, refuses `lash-sqlite-store` and
`rusqlite` in the workspace. Local and test runs that want lash without
PostgreSQL use one file through `SqliteStoreSet::open`, or
`SqliteStoreSet::memory()`, with one process per file
([guide §3](durable-hosting.md#3-sqlite-is-one-file)).

## PostgreSQL topology assumptions

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `k8s/helm/figments/values.yaml:102` | In-cluster `pgvector` PostgreSQL, one instance, no replica (`postgres.enabled: false` by default; production reads `LASH_DATABASE_URL` from its secret). | Choose the topology lash relies on: `Once` survives a failover only if acknowledged commits survive promotion. A single primary or a synchronous standby keeps it; an asynchronous replica does not ([guide §2](durable-hosting.md#durability-across-a-failover)). | decision needed |
| [ ] | `crates/figments-postgres/src/config.rs:139` | `<PREFIX>_TIMEOUT_POLICY=verify-defaults` prepares pools for a transaction-mode pooler (`crates/figments-lash-postgres/src/storage_pool.rs:39`). | Lash's listener needs `LISTEN` and a session advisory lock: connect it directly or in session mode. Through a transaction pooler only, set `Notifier::PollOnly` and accept lease-only crash detection. | decision needed |
| [ ] | `k8s/helm/figments/values.yaml:551` | Connection budget: lash-runtime holds 20 + 20 sqlx connections plus a 16-slot `PostgresStorage` pool per pod, already over-committed against `maxConnections: 400`. | Add five connections per lash node (four reserved, one listener) to the storage pool's `max_connections`. | contract change |
| [ ] | `apps/lash-runtime/src/main.rs:394` | Storage pool `acquire_timeout`, `idle_timeout`, `lock_timeout` and `statement_timeout` from Figments defaults. | Keep a statement timeout, and bound connects, so a hung heartbeat call ends ([guide §1](durable-hosting.md#clean-shutdown)). Set server TCP keepalives so a partitioned node's session ends. | mechanical |
| [ ] | `k8s/helm/figments/values.yaml:562` | KEDA scales lash-runtime 1–6 pods on in-flight LLM calls. | Scale-down must stop the node cleanly (complete `serve`'s stop future on SIGTERM) so its actors are released at once, not after a reap. Set the pod's termination grace above the stop time. | contract change |

## Process retention and projection

| Done | Site | Today | On 1.0 | Mark |
| --- | --- | --- | --- | --- |
| [ ] | `apps/lash-runtime/src/main.rs:669` | `spawn_terminal_process_prune` calls `prune_terminal_process_scopes` on an interval. | That function is gone; prune through the process registry's `prune_terminal_processes`. A terminal process may still have live `Until` children, so prune only what `live_until_descendants` reports ended. | contract change |
| [ ] | `apps/control-plane/crates/workflows/src/projector.rs:62` | The workflow projector reads `process_changes` and treats a process terminal as the run's end, linking child processes (`:197`). | A terminal does not mean the subtree stopped. Decide whether a workflow run ends at its process terminal or when `live_until_descendants` is empty ([guide §8](durable-hosting.md#two-non-guarantees)). | decision needed |

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
  lash (`send()`, facade process and trigger calls) and wait on lash's
  handles, or delete them and submit from the HTTP routes, with cron as a lash
  process or a Figments scheduler. Keeping them keeps a second durable log in
  front of lash.
- **D2. PostgreSQL topology** for the lash database: synchronous standby, a
  single primary without failover, or an asynchronous replica with the
  weakened guarantee written down.
- **D3. Pooler mode** for the lash listener connection: direct or session
  mode, or `Notifier::PollOnly`.
- **D4. Completion secret provisioning and rotation:** which secret store,
  how all pods get the same value, and when a retired version is removed.
- **D5. Adaptive admission against `model_total`:** raise `model_total` to
  cover the 840 s admission budget, or admit before `send()`.
- **D6. Session ownership and observation:** delete the Figments owner lease
  and sticky routing in favour of lash's actor ownership and a shared live
  replay store.
- **D7. Workflow run completion:** the process terminal, or the empty
  `live_until_descendants` subtree.
