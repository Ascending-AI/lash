# 0113: Artifacts are kept alive only by their referrers

## Status

Accepted 2026-09-29 (FIG-4031). It pins the contract for one clean cutover.
Nothing below describes current behaviour unless it cites today's code. Six
implementation lanes build it against this record (§8).

It supersedes the ownership half of
[ADR 0093](0093-artifact-lifetimes-use-exact-owner-edges.md): the
`ArtifactOwner` enum, host/process/execution owners, transfer, release and
execution-only retirement. ADR 0093's exact edges, content verification,
idempotent reclamation and permanent fences stay; fences now cover every
referrer kind. [ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md)
still governs how long a running process lives. It does not govern values.
It amends [ADR 0109](0109-store-to-engine-delivery-is-an-outbox-of-obligations.md)
§1.2 to §1.4 with one kind and one settlement (§2.5), and
[ADR 0112](0112-the-store-is-multi-session-and-a-session-is-resident-from-its-current-frame.md)
§2 and §15 as §5 and §8 say.

Sam's rulings of 2026-09-29, on FIG-4031 and FIG-3116, are binding here:

- Artifacts are kept alive only by their referrers. There is no artifact
  lifetime policy.
- RLM values live in the frame's REPL globals across turns. They are wiped
  only at `continue_as`, and they carry over only by ref in `continue_as`.
  The compaction plugin is standard-only.
- Under the version freeze (FIG-3846), stored shapes change in place.
- Hosts never drive a turn (D5).
- FIG-4031 lands whole: no transitional owner enum, no dual paths, no kept
  queue or drains. This record describes the final state only.

The design it ratifies is
`/workspace/notes/lash/tasks/lanes/study-artifact-referrers.report.md`, with
the inventory in `study-artifact-owners.report.md`. Where this record departs
from the report, *Where the report is refined* says so and why.

## Context

Every citation below was read at fork HEAD `2e29f7e54d`.

**One public owner enum decides lifetimes.** `ArtifactOwner` has three
variants, `Host(String)`, `Process(ProcessId)` and `Execution(ExecutionScope)`
(`crates/lash-core-execution/src/runtime/process/model.rs:183-198`). A start's
staging owner is an `Execution` owner named by a string convention,
`process-start:<start effect id>` (`:216-223`). `storage_parts` gives the
stored `(kind, id)` pair (`:226-245`). The facade exports the enum
(`crates/lash/src/lib.rs:827`), and so do `lash-core`
(`crates/lash-core/src/lib.rs:684`) and `lash-core-execution`
(`crates/lash-core-execution/src/lib.rs:741`).

**Two ports carry owner verbs.** `ModuleArtifactStore` publishes, retains,
transfers, releases and retires
(`crates/lash-core-execution/src/module_artifacts.rs:92-149`).
`ProcessExecutionEnvStore` does the same for environments
(`crates/lash-core-execution/src/runtime/process/model.rs:249-286`). Process
engines get four owner hooks whose defaults do nothing
(`crates/lash-core-execution/src/runtime/process/engine.rs:607-644`); only
lashlang implements them (`crates/lash-lashlang-runtime/src/lib.rs:1268-1319`).

**Storage.** SQLite keeps `artifact_owners` edges and
`artifact_owner_retirements` fences; both CHECK the owner kind, and the fence
admits only `execution` (`crates/lash-sqlite-store/src/schema.rs:571-586`).
PostgreSQL mirrors both (`crates/lash-postgres-store/schema.sql:993-1007`).
Neither checks that an owner id is non-empty; the Rust side refuses only an
empty or NUL-bearing id (`crates/lash-core-store/src/store/namespace.rs:2-4`).
Retirement refuses every non-execution owner on both backends
(`crates/lash-sqlite-store/src/artifact_store.rs:322`,
`crates/lash-postgres-store/src/postgres/artifact_store.rs:458`). PostgreSQL
serializes per owner and per artifact with advisory locks
(`crates/lash-postgres-store/src/postgres/artifact_store.rs:163-198`). SQLite
keeps three databases: durable core, process registry and triggers
(`crates/lash-sqlite-store/src/schema.rs:30-48`). Artifacts, sessions and
named definitions live in the core; processes and triggers do not. PostgreSQL
keeps everything in one database, and its artifact store shares the catalog's
pool (`crates/lash-postgres-store/src/lib.rs:1285-1297`).

**Who publishes, and under what.**

- RLM cells publish modules that export processes under the cell's execution
  scope (`crates/lash-protocol-rlm/src/executor/mod.rs:636-657`, owner from
  `crates/lash-core-execution/src/session/execution_context.rs:639-645`).
  Nothing ties them to the frame whose globals hold them.
- Tool-group formation publishes its environment under the enclosing execution
  scope (`crates/lash-core-execution/src/session/tool_execution/group.rs:271-282`).
- Trigger registration loads the target module and captures an environment
  under the execution owner
  (`crates/lash-lashlang-runtime/src/trigger_commands.rs:94-104`, `:145-148`),
  then stores the references with no subscription edge. Deliveries start
  detached processes from those references later
  (`crates/lash-core-execution/src/triggers/router.rs:645-667`).
- An inherited environment is handed back without publishing
  (`crates/lash-core-execution/src/session/execution_context.rs:1321-1339`).
- A process start stages under the start's owner, registers, transfers to the
  process owner and retires staging
  (`crates/lash-core-execution/src/runtime/process/start_staging.rs:87-176`).
  A refusal retires staging inline (`:109-128`); a coalesced start secures the
  retained record's content first (`:136-172`, `:183-222`). Restate runs the
  whole sequence inside one journaled step
  (`crates/lash-restate/src/controller/process_command.rs:293-320`).
- Hosts publish modules and environments under an owner they choose
  (`crates/lashlang/src/artifact.rs:593-607`,
  `crates/lash-protocol-rlm/src/plugin/factory.rs:287-296`,
  `crates/lash-core-execution/src/runtime/process/model.rs:443-456`).

**Who reclaims.**

- The retirement queue has no producer. `EffectHost` answers an empty list by
  default (`crates/lash-core-execution/src/runtime/effect/executor/control.rs:262-278`),
  the layered host forwards it
  (`crates/lash-core-execution/src/runtime/effect/layered_host.rs:442-453`),
  and Restate queues nothing: session retirement returns 0 and scope retirement
  revokes waits only (`crates/lash-restate/src/effect_host.rs:416-440`).
  `EffectJournalRetirement::Session` names no scope
  (`crates/lash-core-effect/src/retirement.rs:124-134`).
- So the drains that consume it do nothing: SQLite's factory
  (`crates/lash-sqlite-store/src/session_store_factory.rs:34-72`, called at
  `:326`), PostgreSQL's
  (`crates/lash-postgres-store/src/postgres/session_factory/artifact_retirement.rs:4-42`,
  called from `crates/lash-postgres-store/src/postgres/session_factory.rs:43`)
  and session delete's
  (`crates/lash-core/src/runtime/session_delete.rs:326-330`, `:344-367`).
- The facade retires a runtime-operation owner directly once the scope is
  quiescent, and otherwise leaves it to nobody
  (`crates/lash/src/admin.rs:714-735`, `:1334-1351`).
- Process prune stores release inputs in `process_artifact_cleanup` inside the
  registry transaction (`crates/lash-sqlite-store/src/process_registry_change.rs:197-214`),
  and the facade drains them (`crates/lash/src/process_admin.rs:800-846`).
  PostgreSQL's prune writes the same record in its prune statement
  (`crates/lash-postgres-store/src/postgres/process_sql.rs:499-507`). Until
  FIG-4028 (`7fa4b56045`) it dropped the start key, so the drain skipped
  staging retirement; the record now names its staging owner itself
  (`crates/lash-core-execution/src/runtime/process/model/artifact_cleanup.rs:30-40`).

**Frames and values.** RLM globals persist in the execution-state snapshot
root (`crates/lash-protocol-rlm/src/executor/state.rs:18-29`). `continue_as`
clears execution state at its final commit
(`crates/lash-core/src/runtime/turn_boundary.rs:384-398`,
`crates/lash-core/src/runtime/turn_boundary/execution_state.rs:10-24`).
Restore builds a fresh dialect session, applies the snapshot and replays seed
events (`crates/lash-protocol-rlm/src/plugin/runtime_state.rs:137-165`).
Seeds carry JSON and projected values, not artifact edges
(`crates/lash-protocol-rlm/src/projection/transport.rs:14-48`). A frame switch
writes no root terminal (`crates/lash-core/src/runtime/drive.rs:190-194`). The
frame id is derived before effects
(`crates/lash-core-store/src/session_graph.rs:110-125`).

Administrative compaction opens a frame
(`crates/lash-core/src/runtime/session_api.rs:844-851`) and commits the
checkpoint built from resident state (`:979-995`,
`crates/lash-core-store/src/store/mod.rs:804`). Opening a frame does not clear
execution state (`crates/lash-core-store/src/session_state.rs:1676-1720`), and
the glossary says so (`CONTEXT.md:47`). Sam's rule requires the clear.

**Named definitions.** A registry row pins a definition reference
(`crates/lash-core-execution/src/process_registry.rs:55-72`), and resolution
loads its module (`crates/lash-lashlang-runtime/src/lib.rs:1231-1241`). The
registry trait has a CAS write and a list, and no delete
(`crates/lash-core-execution/src/process_registry.rs:163-187`). Nothing keeps a
named definition's module alive.

## Decision

### 1. Referrers and their canonical ids

An artifact has one exact edge per (artifact, referrer) pair. A referrer is a
durable reader. There are seven kinds, and no other way to keep bytes alive.

```rust
// crates/lash-core-store/src/artifact_referrer.rs  (new)
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ArtifactReferrerKind {
    FrameEnvironment,
    ProcessRecord,
    SubscriptionRevision,
    Start,
    Execution,
    HostPin,
    DefinitionRevision,
}

impl ArtifactReferrerKind {
    pub const ALL: [Self; 7];
    /// `frame_environment`, `process_record`, `subscription_revision`,
    /// `start`, `execution`, `host_pin`, `definition_revision`.
    pub const fn as_str(self) -> &'static str;
    pub fn parse(text: &str) -> Result<Self, ArtifactReferrerError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ArtifactReferrer {
    FrameEnvironment(FrameEnvironmentId),
    ProcessRecord(ProcessId),
    SubscriptionRevision(SubscriptionRevisionId),
    Start(StartKey),
    Execution(lash_sansio::EffectJournalIdentity),
    HostPin(HostArtifactPin),
    DefinitionRevision(DefinitionRevisionId),
}

impl ArtifactReferrer {
    pub fn kind(&self) -> ArtifactReferrerKind;
    /// The canonical `referrer_id` text below. Infallible: every id type
    /// validates at construction.
    pub fn canonical_id(&self) -> String;
    /// Typed decode of a stored pair. Refuses an unknown kind, an empty id,
    /// an id that does not decode, and an id whose re-encoding differs from
    /// the stored text. Stores map a refusal to `StoredDataCorrupt`.
    pub fn decode(kind: &str, id: &str) -> Result<Self, ArtifactReferrerError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FrameEnvironmentId { session_id: SessionId, frame_node_id: FrameNodeId }
impl FrameEnvironmentId {
    pub fn new(session_id: SessionId, frame_node_id: FrameNodeId) -> Self;
    pub fn session_id(&self) -> &SessionId;
    pub fn frame_node_id(&self) -> &FrameNodeId;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SubscriptionRevisionId { subscription_id: String, incarnation: String, revision: u64 }
impl SubscriptionRevisionId {
    /// Refuses an empty id or incarnation and revision 0.
    pub fn new(subscription_id: String, incarnation: String, revision: u64)
        -> Result<Self, ArtifactReferrerError>;
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct DefinitionRevisionId { definition_id: String, revision: u64 }
impl DefinitionRevisionId {
    /// `definition_id` is the registry's primary key
    /// (`lash.process-definition:<owner namespace>:<name>`,
    /// `crates/lash-sqlite-store/src/process_definitions.rs:117`). Refuses
    /// an empty id and revision 0.
    pub fn new(definition_id: String, revision: u64) -> Result<Self, ArtifactReferrerError>;
}

/// An opaque, releasable host referrer. Only `mint` makes a new one. Once
/// released, a pin is fenced for good: a host that wants to publish again
/// mints a fresh pin.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct HostArtifactPin(String);
impl HostArtifactPin {
    /// `host-pin:v1:` followed by 32 lowercase hex digits of a random v4 UUID.
    pub fn mint() -> Self;
    pub fn as_str(&self) -> &str;
}
impl TryFrom<String> for HostArtifactPin { type Error = ArtifactReferrerError; }

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum ArtifactReferrerError {
    #[error("unknown artifact referrer kind `{0}`")]
    UnknownKind(String),
    #[error("empty {kind} referrer id")]
    EmptyId { kind: &'static str },
    #[error("malformed {kind} referrer id: {detail}")]
    Malformed { kind: &'static str, detail: String },
    #[error("{kind} referrer id is not canonical")]
    NotCanonical { kind: &'static str },
}
```

All the component types already live in `lash-core-store` or below it
(`crates/lash-core-store/src/session_identity.rs:38`,
`crates/lash-core-store/src/process_identity.rs:84`,
`crates/lash-sansio/src/effect_identity.rs:225`), so the referrer lives there
too and `StoreError` and `ObligationKey` can carry it. `lash-core-execution`
and `lash-core` re-export it for store and engine implementors. The facade
does not: hosts see only `HostArtifactPin` (§2.6).

**Canonical ids.** Every id is non-empty, contains no NUL, and has exactly one
text:

| Kind | `referrer_id` |
|---|---|
| `frame_environment` | compact JSON array `["<session id>","<frame node id>"]` |
| `process_record` | the process id's display form |
| `subscription_revision` | compact JSON array `["<subscription id>","<incarnation>",<revision>]` |
| `start` | the start key (`process-start-key:v1:…`) |
| `execution` | `EffectJournalIdentity::key()`, the journal's existing versioned JSON (`crates/lash-sansio/src/effect_identity.rs:277`) |
| `host_pin` | the pin text |
| `definition_revision` | compact JSON array `["<definition id>",<revision>]` |

JSON arrays are rendered by `serde_json::to_string` of the tuple, so
re-encoding a decoded id reproduces it byte for byte.

**Where the ids come from.**

- `frame_environment`: the session and the `FrameNodeId` of the frame the
  turn was admitted on (`crates/lash-core-store/src/session_graph.rs:110-125`).
- `process_record`: the minted process id (ADR 0107).
- `subscription_revision`: the subscription's deterministic id and the
  incarnation and revision the mutation writes. The new incarnation of
  `Register` and `Revive` stops being random. It is
  `trigger_incarnation(&TriggerOwnerScope, operation_id)`, a framed BLAKE3
  digest, so the effect that runs the command knows the revision id before
  the command commits (§3.4). Every other mutation keeps the incarnation, and
  the revision is `expected_revision + 1`
  (`crates/lash-core-execution/src/triggers.rs:1223-1276`).
- `start`: the start key. This replaces the `process-start:` execution-owner
  convention.
- `execution`: the journal identity of the scope that ran the work.
- `definition_revision`: the registry row's id and the revision the CAS
  writes (`expected_revision + 1`, or 1 for a new slot).

**Storage, SQLite** (durable core, `crates/lash-sqlite-store/src/schema.rs`).
The two tables are renamed in place, so a catalog from before the cutover
fails its first artifact query on the missing table, as ADR 0108 accepted for
missing columns.

```sql
CREATE TABLE IF NOT EXISTS artifact_referrer_edges (
    namespace     TEXT NOT NULL,
    artifact_ref  TEXT NOT NULL,
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_kind
        CHECK (referrer_kind IN ('frame_environment', 'process_record',
            'subscription_revision', 'start', 'execution', 'host_pin',
            'definition_revision')),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_artifact_referrer_edges_id
        CHECK (length(referrer_id) > 0),
    PRIMARY KEY (namespace, artifact_ref, referrer_kind, referrer_id),
    FOREIGN KEY (namespace, artifact_ref)
        REFERENCES artifact_refs(namespace, artifact_ref) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_artifact_referrer_edges_referrer
    ON artifact_referrer_edges(referrer_kind, referrer_id);

-- Permanent: one row per ended referrer, never deleted.
CREATE TABLE IF NOT EXISTS artifact_referrer_fences (
    referrer_kind TEXT NOT NULL CONSTRAINT ck_artifact_referrer_fences_kind
        CHECK (referrer_kind IN ('frame_environment', 'process_record',
            'subscription_revision', 'start', 'execution', 'host_pin',
            'definition_revision')),
    referrer_id   TEXT NOT NULL CONSTRAINT ck_artifact_referrer_fences_id
        CHECK (length(referrer_id) > 0),
    ended_at_ms   INTEGER NOT NULL,
    PRIMARY KEY (referrer_kind, referrer_id)
);
```

**Storage, PostgreSQL** (`crates/lash-postgres-store/schema.sql`). The same
tables as `lash_artifact_referrer_edges` and `lash_artifact_referrer_fences`,
with `lash_lashlang_artifacts` as the foreign-key target, the same CHECK names,
`CHECK (char_length(referrer_id) > 0)`, `ended_at_ms BIGINT NOT NULL`, and
the index `idx_lash_artifact_referrer_edges_referrer`. The open-time shape
check (`crates/lash-postgres-store/src/postgres/schema_shape/`) and
`crates/lash-postgres-store/teardown.sql` name the new tables and drop the
old ones, so a catalog from before the cutover is refused at open.

Both backends render the shared statements from
`crates/lash-store-sql/src/artifact/referrer_edges.rs` and
`crates/lash-store-sql/src/artifact/referrer_fences.rs`, which replace
`owners.rs` and `owner_retirements.rs` (ADR 0098). Every read of an edge or a
fence decodes the pair through `ArtifactReferrer::decode`; a refusal is
`StoredDataCorrupt`, never a skipped row.

### 2. Store operations

#### 2.1 The two ports

Every write names a claim: the referrer plus, for a guarded kind, the guard
its first acquisition arms (§2.4).

```rust
// crates/lash-core-store/src/artifact_referrer.rs
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferrerClaim { referrer: ArtifactReferrer, guard: Option<ArtifactCleanupPlan> }
impl ReferrerClaim {
    /// Unguarded kinds: `frame_environment`, `process_record`, `host_pin`.
    pub fn unguarded(referrer: ArtifactReferrer) -> Result<Self, ArtifactReferrerError>;
    /// `Execution` with `AwaitJournal`, `Start` with `AwaitStart`,
    /// `SubscriptionRevision` with `AwaitSubscriptionRevision`,
    /// `DefinitionRevision` with `AwaitDefinitionRevision`. Any other pairing,
    /// and every `Ended` plan, is refused.
    pub fn guarded(referrer: ArtifactReferrer, guard: ArtifactCleanupPlan)
        -> Result<Self, ArtifactReferrerError>;
    pub fn referrer(&self) -> &ArtifactReferrer;
    pub fn guard(&self) -> Option<&ArtifactCleanupPlan>;
}
```

`ModuleArtifactStore` and `ProcessExecutionEnvStore` keep their namespaces
and their reads. Their owner verbs are replaced by the same four verbs.
Transfer, retain, release and retire are deleted.

```rust
// crates/lash-core-execution/src/module_artifacts.rs
#[async_trait::async_trait]
pub trait ModuleArtifactStore: Send + Sync {
    fn pause_next_publication_for_testing(&self) -> Option<ArtifactPublicationPause>; // unchanged
    fn durability_tier(&self) -> DurabilityTier;                                    // unchanged

    /// Store `bytes` under `module_ref` if absent, verify they equal any
    /// stored bytes, and add the claim's edge, in one transaction that first
    /// takes the referrer's lock and checks its fence (`ReferrerEnded`). The
    /// same transaction inserts the claim's guard row if it has one and no
    /// row exists (§2.4).
    async fn publish_module_artifact(&self, claim: &ReferrerClaim, module_ref: &str,
        bytes: &[u8]) -> Result<(), ArtifactStoreError>;

    /// Add the claim's edge to bytes already stored, with the same lock,
    /// fence check and guard arming. Absent bytes are `ArtifactMissing`.
    async fn acquire_module_artifact(&self, claim: &ReferrerClaim, module_ref: &str)
        -> Result<(), ArtifactStoreError>;

    /// Apply one resolved cleanup in one transaction (§2.3).
    async fn end_module_referrer(&self, cleanup: &ResolvedArtifactCleanup)
        -> Result<(), ArtifactStoreError>;

    async fn get_module_artifact(&self, module_ref: &str)
        -> Result<Option<Vec<u8>>, ArtifactStoreError>;                              // unchanged
}

// crates/lash-core-execution/src/runtime/process/model.rs
#[async_trait::async_trait]
pub trait ProcessExecutionEnvStore: Send + Sync {
    async fn publish_process_execution_env(&self, claim: &ReferrerClaim,
        env_ref: &ProcessExecutionEnvRef, bytes: &[u8]) -> Result<(), ArtifactStoreError>;
    async fn acquire_process_execution_env(&self, claim: &ReferrerClaim,
        env_ref: &ProcessExecutionEnvRef) -> Result<(), ArtifactStoreError>;
    async fn end_process_env_referrer(&self, cleanup: &ResolvedArtifactCleanup)
        -> Result<(), ArtifactStoreError>;
    async fn get_process_execution_env(&self, env_ref: &ProcessExecutionEnvRef)
        -> Result<Option<Vec<u8>>, ArtifactStoreError>;
}
```

The environment port now answers `ArtifactStoreError`, like the module port,
so both classify one way. The free helper `publish_process_execution_env(store,
claim, spec) -> Result<ProcessExecutionEnvRef, PluginError>`
(`crates/lash-core-execution/src/runtime/process/model.rs:443`) keeps its
shape, taking `&ReferrerClaim`.

#### 2.2 Process engines

`ProcessEngine` loses `protect_start_artifacts`, `transfer_start_artifacts`,
`release_artifacts` and `retire_artifact_owner`
(`crates/lash-core-execution/src/runtime/process/engine.rs:607-644`). It gains
three required methods with no defaults, so an engine cannot silently hold
nothing:

```rust
/// Every artifact a start payload names, with the store that holds it.
fn start_artifacts(&self, payload: &serde_json::Value)
    -> Result<Vec<ArtifactName>, crate::PluginError>;

/// Apply a resolved cleanup to the engine's own artifact store. An engine
/// whose artifacts all live in a store-set port answers `Ok(())`: lashlang
/// names only `ArtifactStoreId::LashlangModule`, which the module port ends.
async fn end_artifact_referrer(&self, cleanup: &ResolvedArtifactCleanup)
    -> Result<(), crate::PluginError>;

/// Add the claim's edge to one artifact this engine's store holds, refusing
/// a fenced referrer with `ReferrerEnded`. Called only for names
/// `start_artifacts` reported under `ArtifactStoreId::Engine`, and only after
/// the caller armed the claim's guard with `ArtifactCleanupLedger::arm_cleanup`
/// (an engine store cannot write the store set's ledger in its transaction).
async fn acquire_engine_artifact(&self, claim: &ReferrerClaim, artifact_ref: &str)
    -> Result<(), crate::PluginError>;
```

`ProcessEngineRegistry::{retire_artifact_owner, release_process_artifacts,
release_pruned_process_artifacts}` (`:849-888`) are replaced by one
`end_artifact_referrer(&self, cleanup: &ResolvedArtifactCleanup)` that calls
every installed engine.

#### 2.3 Cleanup records and resolved cleanups

```rust
// crates/lash-core-store/src/artifact_referrer.rs
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(tag = "store", content = "kind", rename_all = "snake_case")]
pub enum ArtifactStoreId {
    ProcessEnv,
    LashlangModule,
    /// A process engine's own store, by engine kind.
    Engine(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct ArtifactName { pub store: ArtifactStoreId, pub artifact_ref: String }

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactCarry { pub artifact: ArtifactName, pub to: ArtifactReferrer }

/// The durable body of one cleanup obligation.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactCleanup {
    pub referrer: ArtifactReferrer,
    pub plan: ArtifactCleanupPlan,
    /// Sever nothing while this journal may still replay: the one execution
    /// that can still read the ended referrer's artifacts (§4.1).
    pub gate: Option<lash_sansio::EffectJournalIdentity>,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "plan", rename_all = "snake_case")]
pub enum ArtifactCleanupPlan {
    /// The referrer has ended. Carry, then fence and sever.
    Ended { carries: Vec<ArtifactCarry> },
    /// Guard of an execution referrer: ends when its journal is settled.
    AwaitJournal,
    /// Guard of a start referrer (§3.3).
    AwaitStart { starter: lash_sansio::EffectJournalIdentity },
    /// Guard of a subscription revision acquired before its mutation commits.
    AwaitSubscriptionRevision { creator: lash_sansio::EffectJournalIdentity },
    /// Guard of a definition revision acquired before its CAS commits.
    AwaitDefinitionRevision { creator: lash_sansio::EffectJournalIdentity },
}

/// What one store applies once the executor has resolved the plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedArtifactCleanup {
    pub referrer: ArtifactReferrer,
    /// Only the carries whose `artifact.store` is the receiving store.
    pub carries: Vec<ArtifactCarry>,
}
```

`end_*_referrer` does four things in one transaction of its store:

1. Insert the referrer's fence (idempotent).
2. For each carry, in `artifact_ref` order:
   - if the destination has a fence, skip it; the destination's own cleanup
     owns what it held;
   - else if the bytes are stored, insert the destination's edge
     (idempotent);
   - else fail with `CarryArtifactMissing`.
3. Delete every edge of the ended referrer.
4. Reclaim every artifact from step 3 that has no edge left, with the
   existing `NOT EXISTS` deletes
   (`crates/lash-sqlite-store/src/artifact_store.rs:43-50`,
   `crates/lash-postgres-store/src/postgres/artifact_store.rs:14-31`) and, on
   SQLite, the blob reclaim that checks every other root
   (`crates/lash-sqlite-store/src/blobs.rs:49`).

Replaying an applied cleanup is a no-op. On PostgreSQL the transaction takes
the referrer locks of the ended referrer and of every carry destination
(key `lash-artifact-referrer:<kind>:<id>`, sorted), then the artifact locks
(`lash-artifact:<namespace>:<ref>`, sorted). Publish and acquire take the
referrer lock, then the artifact lock, as today
(`crates/lash-postgres-store/src/postgres/artifact_store.rs:215-220`). SQLite
serializes on its one writer.

#### 2.4 Where cleanup records live, and the two ways they arise

`artifact_cleanup_obligations` holds cleanup records. It carries ADR 0109's
obligation columns and CHECK (`ck_artifact_cleanup_obligations_obligation`,
§1.1 of that record), its three indexes, and:

```sql
referrer_kind TEXT NOT NULL CHECK (referrer_kind IN (<the seven kinds>)),
referrer_id   TEXT NOT NULL CHECK (length(referrer_id) > 0),  -- char_length on PostgreSQL
cleanup_json  TEXT NOT NULL,   -- ArtifactCleanup, serde JSON
PRIMARY KEY (referrer_kind, referrer_id)
```

`obligation_state` is `NOT NULL` here: a row exists only while it owes a
cleanup. On PostgreSQL there is one table, `lash_artifact_cleanup_obligations`.
On SQLite the table exists in the durable core and in the process registry,
because process prune ends its referrer in the registry's own transaction
(§3.2). The trigger database needs none (§3.4).

A record arises in one of two ways.

- **An end hook** writes `Ended` in the transaction that makes the
  referrer's end durable, beside its record. The write is an upsert: `Ended`
  replaces a guard plan, and a guard never replaces `Ended`. When the end
  hook's transaction is in the artifact store's database, it also inserts the
  fence (and, for a frame switch, the carried edges) in that transaction.
- **A guard** is armed by the first acquisition of a referrer whose record may
  never commit: `execution`, `start`, and a `subscription_revision` or
  `definition_revision` acquired before its mutation commits. Publish and
  acquire insert the guard row (if absent) in the artifact store's own
  transaction, before the bytes are durable. This is the operation-owned
  publication record the report asks for: no edge of a guarded kind exists
  without a durable record that will end it. A guard's delivery asks the
  referrer's authority and ends it only when the authority says it has ended.

Guards live in the durable core on SQLite, beside the edges they guard.

#### 2.5 The one executor

`ObligationKind` gains `ArtifactCleanup` (label `artifact_cleanup`), and
`ObligationKey` gains `ArtifactCleanup { referrer: ArtifactReferrer }`
(`crates/lash-core-store/src/store/obligation.rs:23-110`). Its ledger is
answered by `StoreSet::obligation_ledger(ObligationKind::ArtifactCleanup)` and,
for the extra verbs, by a new required port:

```rust
// crates/lash-core-store/src/store/artifact_cleanup.rs  (new)
#[async_trait::async_trait]
pub trait ArtifactCleanupLedger: ObligationLedger {
    /// Upsert under the rule of §2.4, arming the row `due` at `now_ms`: a
    /// guard plan inserts only when no row exists, and `Ended` replaces a
    /// guard. Callers arm a guard here before acquiring in an engine store.
    async fn arm_cleanup(&self, cleanup: &ArtifactCleanup, now_ms: u64)
        -> Result<ObligationId, StoreError>;
    /// Make an existing row due now. `false` if there is none. An end fact
    /// outside the artifact database calls this after its commit; it only
    /// shortens a guard's wait and never decides anything.
    async fn nudge(&self, referrer: &ArtifactReferrer, now_ms: u64) -> Result<bool, StoreError>;
    async fn load_cleanup(&self, id: &ObligationId) -> Result<Option<ArtifactCleanup>, StoreError>;
}

// crates/lash-core-execution/src/backend.rs, on StoreSet: required
fn artifact_cleanup(&self) -> Arc<dyn ArtifactCleanupLedger>;
```

On SQLite the ledger composes the core and registry tables. It mints ids as
`core:<uuid>` and `registry:<uuid>` and routes `claim`, `settle` and `rearm`
by the prefix. `settle(Delivered)` deletes the row: the fences in every
artifact store are the permanent evidence, and a delivered row has no other
use. This is the one departure from ADR 0109 §1.1 for this kind.

ADR 0109 gains one settlement and one delivery answer:

```rust
// crates/lash-core-store/src/store/obligation.rs
pub enum ObligationSettlement {
    Delivered,
    Retry { due_at_ms: u64, error: String },
    Stall { reason: StallReason, error: String },
    /// Not owed yet: back to `due` at `due_at_ms` with attempts reset to 0.
    Defer { due_at_ms: u64 },
}
// crates/lash-core-execution/src/runtime/drive/relay.rs
pub enum DeliveryFailure {
    Retryable(String),
    Refused(String),
    Undecodable(String),
    /// Settles as `Defer` at `now + policy.max_backoff_ms`. Never stalls.
    NotYet,
}
```

The executor is `ArtifactCleanupRelay`
(`crates/lash-core/src/runtime/artifact_cleanup.rs`, new), assembled by
`obligation_relays` (`crates/lash-core/src/runtime/drive/relays.rs:139`) like
every other kind. It is the only code that severs edges outside an end hook's
own transaction. Its `deliver`:

1. Load the record. A missing row is `Ok` (another relay settled it).
2. If `gate` is set and `EffectHost::journal_replay(gate)` answers
   `MayReplay`, answer `NotYet`.
3. Resolve the plan to carries, or to `NotYet`:
   - `Ended { carries }`: those carries.
   - `AwaitJournal`: no carries once the referrer's own journal is `Settled`.
   - `AwaitStart { starter }`: `ProcessRegistry::get_process_by_start_key`.
     A record means carries of `record.env_ref` (store `ProcessEnv`) and
     `engine.start_artifacts(payload)` for an `Engine` input, all to
     `ProcessRecord(record.id)`. No record means no carries once `starter` is
     `Settled`.
   - `AwaitSubscriptionRevision { creator }`: see §3.4.
   - `AwaitDefinitionRevision { creator }`: see §3.6.
4. Call `end_process_env_referrer`, `end_module_referrer` and
   `ProcessEngineRegistry::end_artifact_referrer`, each with the carries for
   its store.
5. Answer `Ok` only after all of them succeed. The relay settles `Delivered`.

A store fault anywhere is `Retryable`, and the next attempt repeats every step
idempotently. `CarryArtifactMissing` is `Refused`, so the row stalls and is
surfaced (ADR 0109 §1.5). It means an invariant of §3 was broken, and nobody
may paper over it. No partial success is ever acknowledged.

The engine verdict is a new required `EffectHost` method:

```rust
// crates/lash-core-execution/src/runtime/effect/executor/control.rs
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalReplay { MayReplay, Settled }

async fn journal_replay(&self, journal: &lash_sansio::EffectJournalIdentity)
    -> Result<JournalReplay, RuntimeError>;
```

`Settled` is the engine's promise that nothing will replay or append to the
journal again. Restate (`crates/lash-restate/src/effect_host.rs`) answers:

- `Turn` and `QueueDrain`: `Settled` once the root has terminal evidence and
  Restate holds no invocation of that root's drive that is not completed.
- `Process`: `Settled` once the process is terminal and Restate holds no run
  of any of its segments. This is the query ADR 0110 §2's lost-run reconcile
  already makes.
- `RuntimeOperation`: `Settled` once the scope's durable waits are retired
  under `WhenQuiescent`.
- `SessionDelete`: always `Settled`; no publication runs under it.

A wait retirement alone is never `Settled`: it does not prove journal
retirement (`crates/lash-restate/src/effect_host.rs:416-440`).

#### 2.6 Hosts

The host API takes an opaque pin, never a referrer:

```rust
// crates/lash/src/artifacts.rs  (new); LashCore::host_artifacts() -> HostArtifacts
impl HostArtifacts {
    pub async fn publish_module(&self, pin: &HostArtifactPin, artifact: &lashlang::ModuleArtifact)
        -> Result<()>;
    pub async fn publish_process_env(&self, pin: &HostArtifactPin, spec: &ProcessExecutionEnvSpec)
        -> Result<ProcessExecutionEnvRef>;
    /// Ends the pin: fence and `Ended` record in one core transaction. The
    /// executor severs every edge the pin holds in every store. The pin can
    /// never publish again.
    pub async fn release(&self, pin: HostArtifactPin) -> Result<()>;
}
```

The facade exports `HostArtifactPin` and `HostArtifacts` in place of
`ArtifactOwner`. `RlmProtocolPluginFactory::publish_lashlang_module`
(`crates/lash-protocol-rlm/src/plugin/factory.rs:287-296`) is deleted; its
callers move to `HostArtifacts::publish_module`.

#### 2.7 Typed errors

`ArtifactStoreError` (`crates/lash-core-execution/src/module_artifacts.rs:32-52`)
changes in place:

```rust
pub enum ArtifactStoreError {
    Encode(String),
    Decode(String),
    /// Publish or acquire named a referrer that has a fence.
    ReferrerEnded { referrer: ArtifactReferrer },
    /// Acquire named bytes that are not stored.
    ArtifactMissing { artifact_ref: String },
    /// A carry's bytes were gone when its cleanup ran.
    CarryArtifactMissing { artifact_ref: String, to: ArtifactReferrer },
    /// Different bytes under a stored reference.
    Immutable { artifact_ref: String },
    Backend(String),
}
```

`OwnerRetired`, `DestinationOwnerRetired` and `StagingEdgeMissing` go. So do
their `StoreError` twins (`crates/lash-core-store/src/store/error.rs:744-753`),
replaced by `ArtifactReferrerEnded { referrer: ArtifactReferrer }`,
`ArtifactMissing { artifact_ref: String }` and `ArtifactCarryMissing {
artifact_ref: String, to: ArtifactReferrer }`. The `RuntimeErrorCode`s
`ArtifactOwnerRetired`, `ArtifactDestinationOwnerRetired` and
`ArtifactStagingEdgeMissing` (`crates/lash-core-store/src/runtime_error.rs:28-36`)
become `ArtifactReferrerEnded` (`artifact_referrer_ended`) and
`ArtifactMissing` (`artifact_missing`). One classifier replaces the three
predicates of `crates/lash-core-execution/src/runtime/process/model.rs:356-394`:

```rust
pub fn artifact_referrer_ended(error: &crate::PluginError) -> Option<&ArtifactReferrer>;
```

### 3. End hooks

Each subsection names the referrer's acquisitions, the fact that ends it, the
transaction that fact lives in, and where the cleanup record comes from.

#### 3.1 `frame_environment`: frame commit, session deletion, fork

**Acquisition.** Only a turn admitted on frame F writes F's environment.

- An RLM cell that publishes a module publishes it under
  `Execution(turn journal)` and then acquires `FrameEnvironment(S, F)`
  (`crates/lash-protocol-rlm/src/executor/mod.rs:636-657`). The executor's
  published-module cache is keyed by `(FrameEnvironmentId, ModuleRef)`, so a
  module first bound in a new frame acquires that frame's edge.
- At the end of every cell, the executor acquires F's edge for every module
  reachable from a global the cell bound or reassigned, using
  `lashlang::referenced_module_refs(&Value) -> BTreeSet<ModuleRef>` (new).
  This covers definition values that tools return.
- Overwritten values keep their edges until F ends. That retention is
  conservative, and the soft warning is not a memory bound
  (`crates/lash-protocol-rlm/src/plugin/config.rs:29-30`).

So every artifact that a global of F references has an F edge. Call this
**I-frame**. It is what makes carries safe.

**`continue_as`.** The final commit of the switching turn
(`crates/lash-core/src/runtime/turn_boundary.rs:384-398`) asks the code
executor for its carries:

```rust
// CodeExecutorPlugin, crates/lash-core-execution/src/plugin/protocol.rs: required
async fn frame_switch_carries(&self, ctx: ProtocolSessionContext<'_>,
    initial_nodes: &[SessionAppendNode]) -> Result<Vec<ArtifactName>, SessionError>;
```

RLM answers the modules that the seed values in `initial_nodes` reference
(the `continue_as` seed, `crates/lash-protocol-rlm/src/projection/transport.rs:14-48`).
`ExecutionStateUpdate::Clear` becomes `Clear { carries: Vec<ArtifactName> }`.
`RuntimeCommit` gains:

```rust
// crates/lash-core-store/src/store/mod.rs
pub frame_transition: Option<FrameTransition>,

pub struct FrameTransition {
    pub ended: FrameEnvironmentId,
    pub successor: FrameEnvironmentId,
    pub carries: Vec<ArtifactName>,
    /// The committing execution: the only one that can still read `ended`.
    pub gate: lash_sansio::EffectJournalIdentity,
}
```

The backend's session commit transaction
(`crates/lash-sqlite-store/src/persistence/session_commit.rs:340-352`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:280-292`)
does all of this with the head CAS:

1. Every carried artifact must have an edge of `ended`. If one does not, the
   commit is refused with `ArtifactCarryMissing` (I-frame is broken).
2. Insert the successor's edges.
3. Insert `ended`'s fence.
4. Upsert `Ended { carries: [] }` for `ended` with `gate`. The carries were
   applied in step 2, and frame edges live only in the store set's two ports,
   which share this database.

**Every other committed frame open ends the frame too.** Administrative
compaction (`crates/lash-core/src/runtime/session_api.rs:782`, commit at
`:979-995`), direct `open_agent_frame` (`:763`) and standard overflow recovery
all commit a `FrameTransition` with no carries. `open_agent_frame_in_state_with_clock`
(`crates/lash-core-store/src/session_state.rs:1676`) clears the execution-state
snapshot when it opens a frame. After the commit, the runtime restores the
code executor from the cleared state
(`crates/lash-protocol-rlm/src/plugin/runtime_state.rs:137-165`), so the live
globals match the durable ones. Under Sam's rule the compaction plugin is
standard-only, so an RLM session changes frames only through `continue_as`;
the clear makes the rule hold on every path anyway. `CONTEXT.md:47` is
rewritten to say every committed frame switch clears execution state and only
`continue_as` carries values.

**Session deletion.** The catalog's `delete_session` transaction
(`crates/lash-sqlite-store/src/session_deletion.rs`,
`crates/lash-postgres-store/src/postgres/session_factory.rs`) fences
`FrameEnvironment(S, head.current_frame_node_id)` and upserts `Ended {
carries: [] }` with no gate: the `CloseSession` intent has closed every root
first (ADR 0108 §5a). Earlier frames of S were ended at their switches.

**Fork.** `fork_session` restores the retained checkpoint at the fork node
(`crates/lash-sqlite-store/src/forks.rs:83-142`), whose globals belong to the
ancestor's frame F. If `FrameEnvironment(A, F)` has no fence, the fork
transaction copies every edge of `(A, F)` to `(B, F)`. If it has one, those
values were wiped by rule, and the fork's head checkpoint keeps no
execution-state components. Both cases stay in one transaction, because
sessions and edges share a database.

#### 3.2 `process_record`: engine settlement, registry prune

**Acquisition.** The start's cleanup carries onto it (§3.3). A start whose
`Start(key)` already has a fence publishes or acquires directly under
`ProcessRecord(id)` after the row commits, inside the same journaled
registration step.

**End.** Process prune. The registry's prune transaction upserts `Ended {
carries: [] }` for each pruned id, in the registry's own database
(`crates/lash-sqlite-store/src/process_registry_change.rs:197-214`,
`crates/lash-postgres-store/src/postgres/process_sql.rs:499-507`). It replaces
the `process_artifact_cleanup` row. PostgreSQL's prune also inserts the fence
in that transaction. The record carries no release inputs, because
end-referrer severs by the referrer index. It carries no start key either:
`Start(key)` is ended by its own settlement (§3.3) and never waits on prune,
so the start-key plumbing FIG-4028 added is deleted with the record. FIG-4028's
law, `process_prune_retires_the_start_staging_owner`
(`crates/lash-conformance/src/conformance/process_prune_start_staging.rs`), is
rewritten as acceptance test 10. Terminal
completion ends nothing; the record keeps its inputs until prune, as ADR 0093
did.

#### 3.3 `start`: engine start and replay settlement

**Acquisition.** `register_process_start`
(`crates/lash-core-execution/src/runtime/process/start_staging.rs:87`) stages
every artifact the registration names under `Start(key)`:

- the environment: published when the start carries its spec, and acquired
  when it names an existing or inherited `env_ref`;
- every `ProcessEngine::start_artifacts` name.

Acquiring an inherited environment is what the report's "borrowing alone is
insufficient" requires. The guard `AwaitStart { starter }`, where `starter` is
the journal of the scope running the start, is armed before the first edge:
by the environment acquisition in the store set's database, or, for a start
with no environment, by `ArtifactCleanupLedger::arm_cleanup` before any
engine-store acquisition.

**End.** The start settles in one of three ways. None of them writes an
artifact edge outside the executor.

- **Registered.** The registration transaction commits the row. The journaled
  step then calls `ArtifactCleanupLedger::nudge(Start(key))`. The guard
  resolves to carries onto the key's record: the retained record's content,
  never this attempt's.
- **Refused.** On a terminal refusal before any process holds the key, the
  step upserts `Ended { carries: [] }` for `Start(key)`. That is the
  authoritative abandonment.
- **Lost.** If the starter's journal is gone, the guard resolves to no
  carries once `journal_replay(starter)` is `Settled`.

`secure_retained_env`, `secure_retained_engine_artifacts`, the settle helpers
and the inline retirements (`:109-128`, `:136-222`) are deleted.

#### 3.4 `subscription_revision`: subscription retirement, delivery settlement

**Acquisition.** Before the command commits, the trigger command's journaled
effect (`crates/lash-lashlang-runtime/src/trigger_commands.rs:80-86`) acquires
the pre-computed `SubscriptionRevision` id (§1) on the target module and on
the draft's `env_ref`. That covers `Register`, `Update`, `Revive`, `Enable` and
`Disable`; a `Delete` or `Prune` revision delivers nothing and acquires
nothing. The environment is published under the effect's `Execution` referrer
first when it is not inherited (`:145-148`). The guard `AwaitSubscriptionRevision
{ creator }` is armed with the first acquisition. `TriggerStore::execute_command`
takes the incarnation from `trigger_incarnation` (the fixed-incarnation
evaluator already exists,
`crates/lash-core-execution/src/triggers/mutation.rs:24-44`), so the committed
revision is exactly the acquired id or the command loses its fence. After
commit, the effect nudges the revision it superseded.

**End.** A revision ends when three things hold:

- it is not the subscription's current live revision;
- no delivery reserved under its `(incarnation, revision)` is still unbound
  (`crates/lash-postgres-store/schema.sql:963-972` stores both on each
  delivery);
- its creator's journal is `Settled`.

The guard reads that from the trigger store (`list_subscriptions`,
`list_deliveries_by_subscription_id`,
`crates/lash-core-execution/src/triggers.rs:1826-1854`) and answers `NotYet`
until it holds. `delete_session_subscriptions` needs no hook: a deleted row is
not current. Carries are not needed, because a delivery's start acquires
`Start(key)` on the subscription's environment and module before it binds
(§3.3). Binding is therefore the delivery settlement, and the revision ends
after its last binding.

#### 3.5 `host_pin`: host release

**Acquisition.** `HostArtifacts::publish_*` (§2.6).

**End.** `HostArtifacts::release`: one core transaction inserts the fence and
upserts `Ended { carries: [] }`. An unreleased pin keeps its edges
indefinitely, by design.

#### 3.6 `definition_revision`: definition-registry CAS and deletion

**Acquisition.** The `RegisterProcessDefinition` executor
(`crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:393`)
acquires the pre-computed `DefinitionRevision` on the definition's artifacts
before the CAS, which arms `AwaitDefinitionRevision { creator }`. It gets the
artifacts from `ProcessEngine::start_artifacts` over the definition's payload.
`ProcessDefinitionRegistry` gains
`definition_state(&self, definition_id: &str) -> Result<Option<ProcessDefinitionRecord>, PluginError>`
(required) for the guard to read.

**End.** A revision ends when it is not the slot's current resolvable
revision, and its creator's journal is `Settled`. That covers CAS
replacement, a tombstone, a never-committed CAS, and a slot removed with its
session under the ADR 0049 frontier. After a successful CAS, the executor
nudges the replaced revision. Today no code deletes a slot
(`crates/lash-core-execution/src/process_registry.rs:163-187`); this record
adds no delete verb.

**Consumers.** A process started by name, or a subscription registered by
name, acquires its own `Start(key)` or `SubscriptionRevision` on the module
before it relies on it. If the revision was replaced and severed in between,
the acquisition fails `ArtifactMissing`, and the intent refuses typed rather
than pinning bytes that are gone.

#### 3.7 `execution`: replay settlement

**Acquisition.**

- RLM cell publications (§3.1).
- Tool-group environments
  (`crates/lash-core-execution/src/session/tool_execution/group.rs:271-282`).
- Trigger-registration environments (§3.4).

Each is under the journal identity of the enclosing scope. The first
acquisition arms `AwaitJournal`.

**End.** `journal_replay` answers `Settled`. The transactions that make a
scope terminal nudge its guard where they share its database: a turn's final
commit, a drain's end, and a runtime operation's commit on both backends; a
process's terminal transaction on PostgreSQL. SQLite's registry cannot reach
the core in its transaction, so a process journal's guard is found by the
`NotYet` cadence. The facade's direct retirement
(`crates/lash/src/admin.rs:719-726`) is deleted.

### 4. Ordering rules

Edges and fences protect store writes. The rules below protect the lifecycle
decisions around them.

#### 4.1 A frame switch racing publication

A turn admitted on F is the only writer of F's environment. The switch commit
is F's last turn commit, and its transaction fences F. Any later write of F's
environment then fails `ReferrerEnded`.

That later writer can only be a replay of the switching turn, re-executing a
cell after its commit. Its publication still lands under its own `Execution`
edge. The cell treats `ReferrerEnded` on the frame acquire as "this frame was
switched" and goes on: the globals of F are gone, so no reader needs that
edge.

The switching turn may still replay and read F's artifacts. So F's cleanup
carries `gate = <that turn's journal>`, and the executor severs nothing until
`journal_replay` is `Settled`. Carries into the successor are applied inside
the switch commit, so the successor is protected when it becomes visible.
This is the report's "acknowledge destination protection before activating
the successor", done atomically.

#### 4.2 A crash between publication and the referrer's record

- Frame, fork and host pins: the acquisition and the record share one
  transaction, or the referrer's record already exists before the
  acquisition.
- `start`, `execution`, and a revision acquired before its commit: the guard
  row is written in the acquiring transaction, before the bytes are durable.
  After a crash, the guard's authority settles it: the key's record, the
  engine verdict, the subscription row, or the registry slot.

So no edge ever exists without a durable record that will end it.
`start_staging.rs:102-108`'s window, where a failure after staging and before
registration stranded bytes, is closed.

#### 4.3 Key-coalesced starts

The registry transaction chooses the winner (ADR 0107). The start's cleanup
carries the retained record's `env_ref` and engine artifacts, which it reads
at resolution, onto `ProcessRecord(winner)`. Only then does it fence and sever
`Start(key)`. Content that losing attempts staged under the shared key is
severed with it. A losing attempt that stages after the fence gets
`ReferrerEnded`, and it needs nothing: it returns the retained process.

#### 4.4 A trigger delivery after its creator is gone

Revision edges exist before the revision is visible (§3.4), and they are
independent of the creator's frame, execution and session. Reservation happens
in the trigger store's transaction and matches only live revisions. A
revision's end waits for every delivery reserved under it to bind. Binding
happens after the delivery's start has acquired `Start(key)` on the same
bytes. A delivery that starts detached from stored references
(`crates/lash-core-execution/src/triggers/router.rs:645-667`) therefore
always finds them.

#### 4.5 A cancelling child still reading its inputs

Parent close requests cancellation. It does not wait for quiescence
(`crates/lash-core-execution/src/runtime/process/parent_end.rs:75-86`). The
child's inputs are held by `ProcessRecord(child)` until prune. Prune requires
terminal and retired journals (`crates/lash/src/process_admin.rs:760-783`).
The child's own publications are held by `Execution(child)` until its journal
is `Settled`. No scope close severs anything.

#### 4.6 Cross-store prepare, acknowledge, activate and sever

A referrer's record and the artifacts are in independent stores in two
places: SQLite's registry and trigger databases against its core, and any
process engine with its own artifact store. PostgreSQL, and the frame and
definition paths on SQLite, share one database.

- **Prepare.** Destination protection is acquired in every artifact store
  before the successor record commits. Frame carries do it in the commit
  itself; revisions and starts do it pre-commit under guards. Source edges
  are kept throughout.
- **Activate.** The referrer store's transaction commits the successor and
  the ended referrer's `Ended` record together, or its guard is already
  durable.
- **Acknowledge.** Each artifact store applies the resolved cleanup in its
  own transaction: fence, carry, sever, reclaim. The executor settles the
  record only after every store answered `Ok`.
- **Sever.** It happens only inside that per-store step, after the carries
  that store owes.

A partial success is never acknowledged. A retry repeats every store
idempotently. An aborted preparation (a CAS or trigger mutation that loses,
a start that never registers) is ended by its guard. A pending cleanup does
not block a new switch or publication. A second switch from the successor
fences it; a carry into a fenced destination is skipped; and the second
cleanup carries onward from bytes still held by the first referrer.

### 5. What is deleted

These go in the cutover, with no shim.

- **The public owner enum.** `ArtifactOwner` and its impl
  (`crates/lash-core-execution/src/runtime/process/model.rs:183-246`). Its
  re-exports (`crates/lash-core-execution/src/lib.rs:741`,
  `crates/lash-core/src/lib.rs:684`, `crates/lash/src/lib.rs:827`).
  `RuntimeExecutionContext::artifact_owner`
  (`crates/lash-core-execution/src/session/execution_context.rs:637-645`),
  replaced by `execution_referrer()` and `frame_referrer()`.
- **Owner verbs and helpers.** The retain, transfer, release and retire
  methods of both ports (`crates/lash-core-execution/src/module_artifacts.rs:115-142`,
  `crates/lash-core-execution/src/runtime/process/model.rs:258-280`). The
  owner-retired messages, constructors and predicates, and
  `settle_started_process_execution_env` (`model.rs:288-441`).
  `settle_started_process_engine_artifacts`
  (`crates/lash-core-execution/src/runtime/process/engine.rs:556-579`). The
  four engine hooks (`:607-644`) and the registry's three owner methods
  (`:849-888`). `LashlangArtifacts::{retain_module_artifact,
  transfer_module_artifact, release_module_artifact, retire_module_artifact_owner}`
  (`crates/lashlang/src/artifact.rs:610-652`). The lashlang engine's hooks
  (`crates/lash-lashlang-runtime/src/lib.rs:1268-1319`).
  `RuntimeHandle::retire_artifact_owner`
  (`crates/lash-core/src/runtime/observation.rs:394-404`).
- **The empty retirement queue.** `EffectHost::pending_artifact_owner_retirements`
  and `complete_artifact_owner_retirement`
  (`crates/lash-core-execution/src/runtime/effect/executor/control.rs:262-278`),
  their forwarders (`crates/lash-core-execution/src/runtime/effect/layered_host.rs:442-453`),
  and the conformance code that drives them
  (`crates/lash-conformance/src/conformance/effect_group_host.rs:651-704`).
- **The factory drains.** SQLite's `resume_artifact_owner_retirements`, its
  call in `reclaim_retained_evidence`, the `artifact_stores` field and
  `bind_artifact_stores`
  (`crates/lash-sqlite-store/src/session_store_factory.rs:30`, `:34-72`,
  `:307-317`, `:326-330`). The `effect_host` field, which only the drain read
  (`:29`); `bind_effect_host` keeps its turn-cancel-closure binding
  (`:291-300`). PostgreSQL's
  `crates/lash-postgres-store/src/postgres/session_factory/artifact_retirement.rs`
  (whole file), its `mod` line and call
  (`crates/lash-postgres-store/src/postgres/session_factory.rs:4-5`, `:43-47`),
  `bind_artifact_stores` (`:24-34`), and the `effect_host` and
  `artifact_stores` fields (`crates/lash-postgres-store/src/lib.rs:656`,
  `:671-672`). The trait method `bind_artifact_stores`
  (`crates/lash-core-execution/src/runtime/vocabulary.rs:415-422`). In
  ADR 0112's `DeploymentStore` (§2 of that record), `bind_artifact_stores` is
  deleted and not carried over; `bind_effect_host` stays. Its callers go too
  (`crates/lash/src/core.rs:1070-1073`,
  `crates/lash-core/src/testing/checkpoint_observer.rs:343`,
  `crates/lash-core/src/testing/recording_store.rs:514`,
  `crates/lash-sim/src/crash_matrix/deployment.rs:572`).
- **The session-delete drain.** `retire_artifact_owners` and its call
  (`crates/lash-core/src/runtime/session_delete.rs:326-330`, `:344-367`),
  and `SessionDeleteFailure::Artifacts`.
- **Facade-driven cleanup.** The prune drain
  (`crates/lash/src/process_admin.rs:800-846`) and
  `ProcessPruneReport::artifact_cleanup_acknowledgements`
  (`crates/lash-core-execution/src/runtime/process/registry.rs:25-30`). The
  facade's direct operation-owner retirement
  (`crates/lash/src/admin.rs:719-726`).
- **Prune release inputs.** `ProcessArtifactCleanup`, its `start_staging_owner`, and
  `ProcessArtifactCleanupAck`
  (`crates/lash-core-execution/src/runtime/process/model/artifact_cleanup.rs`,
  whole file). The registry's `pending_process_artifact_cleanup` and
  `complete_process_artifact_cleanup`
  (`crates/lash-core-execution/src/runtime/process/registry_concerns.rs:850-865`,
  `crates/lash-core-execution/src/runtime/process/registry_delegate.rs:693-704`,
  `crates/lash-sqlite-store/src/process_registry/prune_api.rs:101-140`,
  `crates/lash-postgres-store/src/postgres/process_registry.rs:1011-1028`,
  `crates/lash-postgres-store/src/postgres/process_registry/prune_api.rs:60`).
  The tables `process_artifact_cleanup`
  (`crates/lash-sqlite-store/src/schema.rs:1256-1260`) and
  `lash_process_artifact_cleanup` (`crates/lash-postgres-store/schema.sql:829-833`),
  and their statements in `crates/lash-store-sql/src/process/artifact_cleanup.rs`.
- **Start staging dances.** The inline retirements and `secure_retained_*`
  (`crates/lash-core-execution/src/runtime/process/start_staging.rs:109-128`,
  `:136-222`). The `process-start:` execution-owner convention
  (`crates/lash-core-execution/src/runtime/process/model.rs:216-223`).
- **Old storage.** `artifact_owners` and `artifact_owner_retirements` on both
  backends (`crates/lash-sqlite-store/src/schema.rs:571-586`, `:633-634`;
  `crates/lash-postgres-store/schema.sql:993-1007`), with
  `crates/lash-store-sql/src/artifact/owners.rs` and `owner_retirements.rs`.
- **Host owner entry points.** `RlmProtocolPluginFactory::publish_lashlang_module`
  (`crates/lash-protocol-rlm/src/plugin/factory.rs:287-296`).
- **Ownerless and lifetime-policy leftovers.** None exist in code. The
  owner-report's proposed public `Scope | Detached` artifact lifetimes and
  `processes.create` lifetime policy were never built and are withdrawn. No
  artifact write without a referrer survives: ADR 0093 already deleted the
  ownerless APIs, and every remaining write takes an `ArtifactReferrer`.

### 6. FIG-3116 part 1 on this contract

`processes.create({ source, dialect })` is an ordinary journaled tool on the
process-controls plugin (`crates/lash-plugin-process-controls/src/declarations.rs`).
It returns a definition value, the same `ProcessDefinitionRef` shape
`processes.start` takes. Inside its journaled effect, before it records its
result, it:

1. compiles the module (pure, ADR 0093);
2. publishes it under `Execution(the call's journal)`, which arms that guard;
3. acquires `FrameEnvironment(S, F)` for the frame the calling turn was
   admitted on.

A redrive that re-executes the effect repeats both writes idempotently. A
replay that reads the recorded result touches no store. There is no lifetime
argument, no plugin lifetime policy, and no new referrer kind.

The value then lives in the frame's globals and holds F's edge (I-frame). It
survives a switch only when a `continue_as` seed passes it, because
`frame_switch_carries` finds its module. Starting it acquires `Start(key)`
and then `ProcessRecord(id)` (§3.3) and consumes nothing. Registering it by
name acquires a `DefinitionRevision` (§3.6). Start lifetime stays ADR 0108's.
FIG-3116's `triggers.register` half rides §3.4 unchanged.

### 7. Acceptance tests

Store-level laws live in `crates/lash-conformance/src/conformance/artifact_referrers.rs`,
registered by a new `artifact_referrer_tests!` in
`crates/lash-conformance/src/macros.rs`. They drive the store through
lash-restate's engine on the in-process Restate double, which both store
crates already use for conformance (`crates/lash-sqlite-store/Cargo.toml:47`,
`crates/lash-postgres-store/Cargo.toml:59-60`). Each runs on
`//crates/lash-sqlite-store:conformance__test`,
`//crates/lash-sqlite-store:conformance_memory__test` and
`//crates/lash-postgres-store:conformance__test`.

RLM-level cases live in `crates/lash/tests/artifact_referrers_evidence.rs`
(`//crates/lash:artifact_referrers_evidence__test`). They run on SQLite always,
and on PostgreSQL when `LASH_POSTGRES_DATABASE_URL` is set. They fail rather
than skip under `LASH_REQUIRE_POSTGRES=1`, the pattern of
`crates/lash-restate/src/tests/postgres_ingress.rs:74-76`. The PostgreSQL half
runs under `scripts/ci/with-service.sh pg16`.

Every case asserts the exact edge set after each step, eventual reclamation
after the relay drains, and that a second relay pass changes nothing.

1. **Cold-reopen globals across turns** (lash). A cell binds a definition. The
   session parks and reopens cold, and a later turn in the same frame starts
   it. The module has exactly one `FrameEnvironment(S, F)` edge and one
   `Execution(turn 1)` edge. After turn 1's journal settles, only the frame
   edge remains.
2. **Overwrite retention** (lash). Rebinding a global keeps the old module's
   frame edge until the frame ends, and it is reclaimed after the switch.
3. **Carried and un-carried `continue_as`** (lash). Two definitions, one
   passed in the seed. After commit, the carried one has an F2 edge and F1 is
   fenced. After the gate settles, the uncarried one is reclaimed, and the
   carried one is started from F2 successfully.
4. **Administrative compaction clears execution state** (conformance, plus
   `//crates/lash-core:runtime_turns__test`). The compaction commit's
   checkpoint holds no execution-state components, the old frame is fenced,
   its edges are severed after the gate, and the live executor's globals are
   empty.
5. **Publication racing the switch** (conformance). A publication paused at
   its serialization point (`pause_next_publication_for_testing`) is resumed
   after the switch commit. It fails `ReferrerEnded` under the frame and
   succeeds under its execution.
6. **Crash before the record** (conformance). Stage a start under
   `Start(key)` and kill before registration. The guard row exists. With the
   starter's journal `MayReplay`, the relay defers. Once it is `Settled`, the
   staged bytes are reclaimed and the key is fenced. The same holds for a
   trigger mutation that never commits and for a losing definition CAS.
7. **Crash between acknowledgements** (conformance). Fault the engine store's
   `end_artifact_referrer` after the environment store applied. The record
   stays due with the environment store fenced. The retry applies both and
   settles, and no store is acknowledged partially.
8. **Coalesced starts with different inputs** (conformance). Two attempts at
   one key stage different environments, and the registrar keeps the first.
   After the relay: `ProcessRecord` holds exactly the retained content, the
   loser's environment is reclaimed, and `Start(key)` is fenced.
9. **Creator deletion with pending deliveries** (conformance). Register a
   subscription from a session, reserve two deliveries, and delete the
   session. The revision is held until both deliveries bind. Each started
   process loads its environment and module.
10. **Prune and late-transfer fences** (conformance; FIG-4028's law). Prune a
    keyed started process. `ProcessRecord` is fenced and its edges severed on
    both stores, and `Start(key)` was already fenced by settlement. A late
    publish or acquire under either fails `ReferrerEnded`, and a carry into
    the pruned record is skipped.
11. **Cancellation still reading** (conformance). The parent scope closes and
    the child is cancelled but still running. Its record and execution edges
    stay, and its environment loads until it is terminal and pruned.
12. **Named-definition survival** (lash). Register a created definition by
    name, then `continue_as` without carrying it. The frame edge goes after
    the gate. The `DefinitionRevision` edge keeps the module, and a new turn
    starts it by name. Replacing the name ends the old revision after its
    creator settles.
13. **Fork of a live and an ended frame** (conformance). A fork inside a live
    frame copies the frame's edges to the fork's own id. A fork at a pinned
    point whose frame was switched inherits no execution-state components.
14. **Host pins** (conformance). Publish under a pin, release it, and the
    bytes are reclaimed. Publishing again under the released pin fails
    `ReferrerEnded`, and a fresh pin works.
15. **Malformed and old shapes** (conformance, plus
    `//crates/lash-core-store:lash-core-store__unit_test` for encoding).
    - Insert an edge with an empty id: the CHECK refuses it on both backends.
    - A non-canonical, unknown-kind or undecodable id read back is
      `StoredDataCorrupt`.
    - A SQLite catalog with the old `artifact_owners` table fails its first
      artifact query.
    - A PostgreSQL catalog with the old tables fails the open-time shape
      check.
    - Every kind round-trips through `canonical_id` and `decode`.
16. **Retry idempotency** (conformance). Run every delivery twice, including
    after a claim lapses mid-delivery. The edge sets, fences and reclaimed
    rows are identical.
17. **Journal verdict** (`//crates/lash-restate:lash-restate__unit_test`). A
    wait retirement alone answers `MayReplay`. A completed root drive, a
    terminal process with no held segment run, and a quiescent runtime
    operation answer `Settled`.

### 8. Lanes and file ownership

Six lanes. Each is cut from `main` as soon as this record lands. None waits
for FIG-3946.

**Contract first.** Lane R's first commit, **R0**, pins every shared type and
signature:

- `crates/lash-core-store/src/artifact_referrer.rs`, `store/artifact_cleanup.rs`
  and their `mod` lines;
- the `StoreError`, `ArtifactStoreError` and `RuntimeErrorCode` changes;
- `ObligationKind::ArtifactCleanup`, `ObligationKey::ArtifactCleanup` and
  `ObligationSettlement::Defer`;
- `DeliveryFailure::NotYet`;
- `RuntimeCommit::frame_transition` and `FrameTransition`;
- the reshaped `ModuleArtifactStore`, `ProcessExecutionEnvStore` and
  `ProcessEngine`;
- `EffectHost::journal_replay`, `CodeExecutorPlugin::frame_switch_carries`,
  `StoreSet::artifact_cleanup`, `ProcessDefinitionRegistry::definition_state`
  and `trigger_incarnation`;
- the `lash-store-sql` statement sets for edges, fences and cleanup
  obligations;
- `HostArtifactPin` and `ReferrerClaim`.

R0 must pass `kiln clippy` for `//crates/lash-core-store`,
`//crates/lash-store-sql` and `//crates/lash-core-execution`. The rest of the
workspace may stay red until integration. The other five lanes stack on R0
and run in parallel. They integrate once, onto `fig-4031/referrers`, with no
adapters.

| Lane | Owns |
|---|---|
| **R: contract and executor** (R0 first) | `crates/lash-core-store/src/artifact_referrer.rs`; `crates/lash-core-store/src/store/artifact_cleanup.rs`; `crates/lash-core-store/src/store/obligation.rs`; `crates/lash-core-store/src/runtime_error.rs`; `crates/lash-core-store/src/runtime_error/classification.rs`; `crates/lash-store-sql/src/artifact/**`; `crates/lash-core-execution/src/module_artifacts.rs`; `crates/lash-core-execution/src/runtime/process/model.rs`; `crates/lash-core-execution/src/runtime/process/model/artifact_cleanup.rs` (deleted); `crates/lash-core-execution/src/runtime/process/engine.rs`; `crates/lash-core-execution/src/runtime/process/registry.rs`; `crates/lash-core-execution/src/runtime/drive/relay.rs`; `crates/lash-core-execution/src/runtime/effect/executor/control.rs`; `crates/lash-core-execution/src/runtime/effect/layered_host.rs`; `crates/lash-core-execution/src/backend.rs`; `crates/lash-core-execution/src/plugin/protocol.rs`; `crates/lash-core/src/runtime/artifact_cleanup.rs` (new); `crates/lash-core/src/runtime/drive/relays.rs`; `crates/lash-core/src/runtime/observation.rs`; `crates/lash/src/artifacts.rs` (new); `crates/lash/src/process_admin.rs`; `crates/lashlang/src/artifact.rs` |
| **S: SQLite** | `crates/lash-sqlite-store/src/artifact_store.rs`; `crates/lash-sqlite-store/src/obligation_ledger.rs`; `crates/lash-sqlite-store/src/process_definitions.rs`; `crates/lash-sqlite-store/src/process_registry_change.rs`; `crates/lash-sqlite-store/src/process_registry/**`; `crates/lash-sqlite-store/src/triggers.rs`; `crates/lash-sqlite-store/src/triggers/**`; `crates/lash-sqlite-store/src/blobs.rs`; `crates/lash-store-sql/src/process/artifact_cleanup.rs` (deleted); `crates/lash-store-sql/src/process.rs` |
| **P: PostgreSQL** | `crates/lash-postgres-store/src/postgres/artifact_store.rs`; `crates/lash-postgres-store/src/postgres/obligation_ledger.rs`; `crates/lash-postgres-store/src/postgres/process_definitions.rs`; `crates/lash-postgres-store/src/postgres/process_sql.rs`; `crates/lash-postgres-store/src/postgres/process_registry.rs`; `crates/lash-postgres-store/src/postgres/process_registry/**`; `crates/lash-postgres-store/src/postgres/prune.rs`; `crates/lash-postgres-store/src/postgres/trigger_store.rs`; `crates/lash-postgres-store/src/postgres/session_factory/artifact_retirement.rs` (deleted); `crates/lash-postgres-store/teardown.sql` |
| **X: starts, triggers, definitions and engines** | `crates/lash-core-execution/src/runtime/process/start_staging.rs`; `crates/lash-core-execution/src/runtime/process/registry_concerns.rs`; `crates/lash-core-execution/src/runtime/process/registry_delegate.rs`; `crates/lash-core-execution/src/session/execution_context.rs`; `crates/lash-core-execution/src/session/tool_execution/group.rs`; `crates/lash-core-execution/src/triggers.rs`; `crates/lash-core-execution/src/triggers/**`; `crates/lash-core-execution/src/process_registry.rs`; `crates/lash-core-execution/src/tool_dispatch/intent_executor.rs`; `crates/lash-lashlang-runtime/**`; `crates/lash-restate/src/effect_host.rs`; `crates/lash-restate/src/controller/process_command.rs` |
| **F: frames and RLM** | `crates/lash-protocol-rlm/src/executor/**`; `crates/lash-protocol-rlm/src/plugin/runtime_state.rs`; `crates/lash-protocol-rlm/src/plugin/factory.rs`; `crates/lash-protocol-rlm/src/projection/transport.rs`; `crates/lash-core/src/runtime/turn_boundary/execution_state.rs`; `lashlang::referenced_module_refs` in a new `crates/lashlang/src/value_refs.rs`; `CONTEXT.md` |
| **K: acceptance** | `crates/lash-conformance/src/conformance/artifact_referrers.rs` (new) and its fixtures; `crates/lash-conformance/src/conformance/process_prune_start_staging.rs`; `crates/lash-conformance/src/conformance/artifact_store.rs`; `crates/lash-conformance/src/fused_artifact_store.rs`; `crates/lash-conformance/src/conformance/effect_group_host.rs`; `crates/lash/tests/artifact_referrers_evidence.rs` (new) |

A lane edits only the files it owns and the shared files below, in the
regions named. A change another lane needs goes to that lane's owner. Each
lane regenerates the BUILD files of the crates whose files it adds.

**Files ADR 0112's lanes also own.** ADR 0112 assigns whole crates by glob
(its §15). The FIG-4031-owned files above sit inside those globs and are
carved out: ADR 0112's lanes do not edit them. The files that both cutovers
must edit are shared. The ADR 0112 lane stays the owner. The FIG-4031 lane
named writes only the region named, and whichever integration lands second
rebases and resolves. ADR 0112's C0 commit on `fig-1628/cutover` is not
waited on; FIG-4031's regions touch no signature C0 pins except
`DeploymentStore::bind_artifact_stores`, which C0 should omit and FIG-4031
deletes if it is present.

| Shared file | Owner (ADR 0112 lane) | FIG-4031 lane and region |
|---|---|---|
| `crates/lash-core-store/src/store/mod.rs` | runtime | R: `RuntimeCommit::frame_transition` and its builders |
| `crates/lash-core-store/src/store/error.rs` | runtime | R: the artifact variants only |
| `crates/lash-core-store/src/lib.rs` | runtime | R: `mod` and re-export lines |
| `crates/lash-core-store/src/session_state.rs` | runtime | F: the clear in `open_agent_frame_in_state_with_clock` |
| `crates/lash-core-execution/src/lib.rs` | runtime | R: re-export lines |
| `crates/lash-core-execution/src/runtime/vocabulary.rs` | runtime | R: delete `bind_artifact_stores` |
| `crates/lash-core/src/lib.rs` | runtime | R: re-export lines |
| `crates/lash-core/src/runtime/session_api.rs` | runtime | F: compaction and `open_agent_frame` commits carry `FrameTransition` |
| `crates/lash-core/src/runtime/turn_boundary.rs` | runtime | F: carries into `ExecutionStateUpdate::Clear` |
| `crates/lash-core/src/runtime/session_delete.rs` | runtime | R: delete the drain |
| `crates/lash/src/lib.rs` | runtime | R: export swap |
| `crates/lash/src/core.rs` | runtime | R: delete `bind_artifact_stores` call; add `host_artifacts()` |
| `crates/lash/src/admin.rs` | runtime | R: delete direct retirement |
| `crates/lash/BUILD.bazel` | runtime | K: the new test target |
| `crates/lash-sqlite-store/src/schema.rs` | SQLite | S: artifact tables and both cleanup tables |
| `crates/lash-sqlite-store/src/persistence/session_commit.rs` | SQLite | S: the frame-transition block in the commit transaction |
| `crates/lash-sqlite-store/src/session_deletion.rs` | SQLite | S: frame fence and cleanup in the delete transaction |
| `crates/lash-sqlite-store/src/forks.rs` | SQLite | S: edge copy or checkpoint strip |
| `crates/lash-sqlite-store/src/session_store_factory.rs` | SQLite | S: delete the drain and its fields |
| `crates/lash-sqlite-store/src/lib.rs` | SQLite | S: `mod` lines |
| `crates/lash-postgres-store/schema.sql` | PostgreSQL | P: artifact tables and cleanup table |
| `crates/lash-postgres-store/src/postgres/schema_shape/` | PostgreSQL | P: shape entries for those tables |
| `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs` | PostgreSQL | P: the frame-transition block |
| `crates/lash-postgres-store/src/postgres/session_factory.rs` | PostgreSQL | P: delete the drain; frame fence in delete; fork copy |
| `crates/lash-postgres-store/src/lib.rs` | PostgreSQL | P: delete the drain fields |
| `crates/lash-core/src/testing/checkpoint_observer.rs`, `crates/lash-core/src/testing/recording_store.rs` | readers | R: delete `bind_artifact_stores` forwarders |
| `crates/lash-sim/src/crash_matrix/deployment.rs` | readers | R: delete the forwarder |
| `crates/lash-conformance/src/macros.rs` | readers | K: the `artifact_referrer_tests!` block |

**Files FIG-3946 changes.** The squashed integration `f3946-integrate`
(`404b248d28`) changes these files that FIG-4031 lanes also edit. The named
lane rebases over FIG-3946 when it lands.

- **R:** `crates/lash-core-store/src/store/mod.rs`,
  `crates/lash-core-store/src/store/error.rs`,
  `crates/lash-core-store/src/lib.rs`, `crates/lash-core-execution/src/lib.rs`,
  `crates/lash-core-execution/src/runtime/vocabulary.rs`,
  `crates/lash-core/src/lib.rs`, `crates/lash/src/lib.rs`.
- **F:** `crates/lash-core/src/runtime/session_api.rs`,
  `crates/lash-core/src/runtime/turn_boundary.rs`.
- **S:** `crates/lash-sqlite-store/src/schema.rs`,
  `crates/lash-sqlite-store/src/persistence/session_commit.rs`,
  `crates/lash-sqlite-store/src/session_store_factory.rs`,
  `crates/lash-sqlite-store/src/lib.rs`.
- **P:** `crates/lash-postgres-store/schema.sql`,
  `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs`,
  `crates/lash-postgres-store/src/postgres/trigger_store.rs`,
  `crates/lash-postgres-store/src/lib.rs`.

FIG-3946 does not touch `crates/lash-core/src/runtime/session_delete.rs` or
any artifact file.

**Files the FIG-433 capture-store ADR will share.** FIG-433's design
(`/workspace/notes/lash/tasks/lanes/design-433.report.md`) adds a
session-owned capture store and commits a stopped partial in the
cancellation commit. It will edit these files that FIG-4031 lanes also edit:

- `crates/lash-core-store/src/store/mod.rs`
- `crates/lash-core-store/src/store/error.rs`
- `crates/lash-core-store/src/lib.rs`
- `crates/lash-sqlite-store/src/schema.rs`
- `crates/lash-sqlite-store/src/persistence/session_commit.rs`
- `crates/lash-postgres-store/schema.sql`
- `crates/lash-postgres-store/src/postgres/schema_shape/`
- `crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs`
- `crates/lash-restate/src/effect_host.rs`
- `crates/lash/src/lib.rs`
- `crates/lash-conformance/src/macros.rs`

For each, the ADR 0112 lane named above stays the owner.
`crates/lash-restate/src/effect_host.rs` belongs to FIG-4031's lane X, because
ADR 0112 does not edit it. FIG-4031 claims only the regions in the tables
above: the artifact variants, `frame_transition`, the artifact and cleanup
DDL, the frame-transition block, `journal_replay`, and its export and macro
lines. FIG-433's capture segment, capture tables, partial-commit block,
resolver changes and exports are FIG-433's regions. The integration that
lands later rebases.

The final integration merge, and the one full gate on it, belong to the
orchestrator.

## Where the report is refined

- **Carries ride the ended referrer's own cleanup.** The report acknowledges
  destination protection before activating the successor. Here, frame
  carries happen inside the activating commit, which the shared database
  allows. Every other destination is acquired before its record commits,
  under a guard. The code shows why a separate prepare record is not needed:
  the store set's module and environment ports live beside the session
  catalog (`crates/lash-core-execution/src/backend.rs:167-173`), and
  PostgreSQL's artifact store shares the catalog pool
  (`crates/lash-postgres-store/src/lib.rs:1285-1297`).
- **Pending cleanups block nothing.** The report has pending transitions
  block conflicting publication and switches. A carry into a fenced
  destination is skipped and the source is never severed before its own
  carries, so blocking buys no safety (§4.6).
- **A switch cleanup is gated on the switching turn's journal.** The report
  severs at the switch. That would break a post-commit replay of the
  switching turn that reads F's globals, since Restate replays by
  re-execution (ADR 0103).
- **Subscription ends need no carries.** The report ends a revision when its
  last pending delivery settles, which suggests carrying its bytes onward.
  A delivery's start already stages the environment it references under the
  start (`crates/lash-core-execution/src/runtime/process/start_staging.rs:269-288`
  re-publishes a referenced `env_ref` under the staging owner), and §3.3
  keeps that as an acquisition of `Start(key)`. The revision can therefore
  end at its last binding with no carries.
- **Definition consumers do not hold up the revision's end.** A consumer
  acquires first and refuses typed if it lost the race (§3.6). The report's
  "until admitted consumers finish acquiring" is met without the registry
  knowing its consumers.
- **FIG-4028's fix is superseded.** It landed on main (`7fa4b56045`) and made
  PostgreSQL's prune carry the start key to the drain. Here prune no longer
  ends the start, so that plumbing goes with the cleanup record, and its law
  is kept as test 10.

## Consequences

- An artifact is alive exactly while some referrer holds an edge. There is
  no lifetime policy, no count, no lease and no sweep that infers one.
- Every end is durable before any severing, and one relay does all
  severing. It acknowledges only after every store. Failures stall visibly
  under ADR 0109's surfacing.
- Fence rows are permanent, one per ended referrer, like ADR 0108's deletion
  tombstones. That includes one per settled execution journal.
- Guards wait by polling at the relay's maximum backoff unless a nudge
  arrives. SQLite process journals and subscription revisions are the
  polled cases.
- RLM values follow Sam's rule on every path. A committed frame switch clears
  execution state, and only `continue_as` carries values. A fork at an ended
  frame starts without them.
- The facade loses `ArtifactOwner` and
  `ProcessPruneReport::artifact_cleanup_acknowledgements`, and gains
  `HostArtifactPin` and `HostArtifacts`. The facade lane adds
  `//crates/lash:ui_fixtures` to its checks.
- Stored shapes change in place with no version bump: the renamed edge and
  fence tables, the cleanup tables, the dropped `process_artifact_cleanup`,
  deterministic trigger incarnations, and the `RuntimeErrorCode` rename.
  Old catalogs are refused and recreated. A journal in flight at the cutover
  deploy drains on the build that wrote it (ADR 0106).

## Lane G amendment

Amended 2026-09-29 (FIG-4031, lane G). §3.1 ended one frame per commit, and
only when a turn's final commit or a compaction attached a `FrameTransition`.
Two frames therefore kept their edges until session deletion:

- a frame opened only in resident state (a direct `open_agent_frame`) and
  switched away from by the same commit, after an earlier committed frame:
  the transition could end only the head frame;
- the frame left by a direct `open_agent_frame` committed by a park or a
  session command, which attaches no transition.

**Rule.** A commit ends every frame it leaves. The frames it leaves are the
prior head's frame, then each frame whose `FrameOpen` the commit appends, in
graph order, except the frame the new head holds
(`lash_core_store::store::frames_left_by_commit`). The store derives this
chain inside the session commit transaction, from the head it has locked and
the commit's own nodes, and ends each frame in it: its fence, and its
`Ended { carries: [] }` record. The executor names no chain, so no commit
path can leave a frame unended, whatever its origin.

**`FrameTransition` keeps its shape.** It now says only what the store
cannot know: the carries, the frame they leave, and the gate.

- `ended` names the frame whose edges hold the carries: the frame the
  switching turn was admitted on. It must be one of the frames the commit
  leaves (the head's frame, or one this commit opens); otherwise the commit
  is refused. This replaces lane S's first-commit rule (c2540bbff8), which
  is the case where the chain holds only an appended frame.
- The carry check of §3.1 step 1 is unchanged: each carry needs an edge of
  `ended`, or the commit fails `ArtifactCarryMissing`. A `continue_as` seed
  naming a module its frame does not hold fails closed.
- `gate` gates the cleanup of every frame in the chain.

A commit that leaves frames with no transition (a park or a session command
that persists a direct frame open) ends them ungated: no execution is
running on the session, and the turns that read those frames have already
committed. This is the reasoning session deletion uses.

**Locks.** On PostgreSQL the commit takes the referrer locks of every ended
frame and of the successor, sorted by key, then the carried artifacts' locks,
as §2.3 orders them. SQLite serializes on its one writer.
