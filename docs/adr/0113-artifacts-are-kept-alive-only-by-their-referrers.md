# 0113: Artifacts are kept alive only by their referrers

## Status

Accepted. The definition vocabulary depends on FIG-4178, which removes the
definition-revision referrer and the named definition registry.

## Context

A frame's globals, a process record, a subscription revision and a replaying
execution can read the same immutable artifact at different times. Each
reader needs its own durable edge. Process lifetime is a separate decision
([ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md)); a
process's completion does not itself release the inputs its record holds.
The cleanup obligation records when a reader ends, and the engine decides
when its journal cannot replay
(`crates/lash-core/src/runtime/artifact_cleanup.rs:28-73`).

## Decision

### 1. Referrers and their canonical ids

An artifact has one exact edge per artifact/referrer pair. Six kinds hold
artifacts: `frame_environment`, `process_record`, `subscription_revision`,
`start`, `execution` and `host_pin`. `session` and `upload` hold attachments
only ([ADR 0124](0124-attachments-are-kept-alive-only-by-their-referrers.md)).
An artifact reference or definition id alone keeps no bytes alive.

`ArtifactReferrer` lives in `lash-core-store`. Its canonical encodings are:

| Kind | Canonical `referrer_id` |
|---|---|
| `frame_environment` | Compact JSON array `["<session id>","<frame node id>"]` |
| `process_record` | The process id's display form |
| `subscription_revision` | Compact JSON array `["<subscription id>","<incarnation>",<revision>]` |
| `start` | The start key's text |
| `execution` | `EffectJournalIdentity::key()` |
| `host_pin` | `host-pin:v1:` and 32 lowercase hex digits of a random v4 UUID |
| `session` | The session id's text |
| `upload` | Compact JSON array `["<session id>","upload:v1:<32 hex>"]` |

Decode refuses an empty or NUL-bearing id, an unknown kind, an undecodable
id, and an id whose re-encoding differs from the stored text. Unknown kinds
are `Incompatible(UnknownVocabulary)` under
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md);
malformed ids are `StoredDataCorrupt`. A store never treats an undecodable
edge as absent (`crates/lash-core-store/src/artifact_referrer.rs:176-279,621-633`).

A frame id combines the session and the admitted frame node. A subscription
revision combines its subscription id, incarnation and positive revision.
`Register` and `Revive` derive their incarnation with `trigger_incarnation`
from the owner scope and operation id; other mutations keep the incarnation
and increment the expected revision. The effect therefore knows the
revision it acquires before the mutation commits
(`crates/lash-core-execution/src/triggers/revision_referrer.rs:64-151`,
`crates/lash-core-execution/src/triggers/mutation.rs:408-413`).

SQLite stores exact edges in `artifact_referrer_edges` and permanent fences
in `referrer_fences`. Edges have a composite primary key and a foreign key
to the namespaced artifact reference; referrer ids must be nonempty.
PostgreSQL uses `lash_artifact_referrer_edges` and `lash_referrer_fences`.
The shared SQL statements live in `lash-store-sql`
(`crates/lash-sqlite-store/src/schema.rs:633-647`,
`crates/lash-postgres-store/schema.sql:1075-1089`,
`crates/lash-store-sql/src/artifact/referrer_edges.rs:1`,
`crates/lash-store-sql/src/artifact/referrer_fences.rs:1`).

### 2. Store operations

#### 2.1 The two ports

`ModuleArtifactStore` and `ProcessExecutionEnvStore` expose publish,
acquire, end-referrer and read operations. Every publish or acquire takes a
`ReferrerClaim`. Publish verifies identical bytes under an existing
reference and adds the exact edge. Acquire requires stored bytes. Both
check the referrer's fence and arm the claim's guard in the same store
transaction (`crates/lash-core-execution/src/module_artifacts.rs:158-191`,
`crates/lash-core-execution/src/runtime/process/model.rs:228-268`,
`crates/lash-sqlite-store/src/artifact_store.rs:198-283`,
`crates/lash-postgres-store/src/postgres/artifact_store.rs:182-254`).

A claim is unguarded for a frame, process record or host pin. Guarded
claims pair an execution with `AwaitJournal`, a start with `AwaitStart`,
a subscription revision with `AwaitSubscriptionRevision`, or a prepared
successor frame with `AwaitFrame`. Invalid pairings and `Ended` guards
are refused. Attachment upload guards belong to ADR 0124
(`crates/lash-core-store/src/artifact_referrer.rs:640-730`).

#### 2.2 Process engines

Every `ProcessEngine` must implement `start_artifacts`,
`end_artifact_referrer` and `acquire_engine_artifact`. The engine reports
all artifacts its start payload names and identifies their stores. An
engine whose artifacts live in the store set has no separate store to end.
The registry sends every installed engine the ended referrer and only that
engine's carries (`crates/lash-core-execution/src/runtime/process/engine.rs:563-592,812-828`).

Before acquiring in an engine's independent store, `ArtifactReferrerPorts`
arms the claim's guard through `ArtifactCleanupLedger`. The engine cannot
write that ledger inside its own transaction
(`crates/lash-core-execution/src/runtime/process/start_staging.rs:109-180`).

#### 2.3 Cleanup records and resolved cleanups

`ArtifactName` identifies an artifact and its store: `ProcessEnv`,
`LashlangModule`, `ProcessDefinition`, or `Engine(kind)`. `ArtifactCarry`
names a destination referrer. `ArtifactCleanup` is the durable obligation
body, with a referrer, plan and optional replay gate.
`ResolvedArtifactCleanup` contains only the receiving store's carries,
ordered by artifact reference
(`crates/lash-core-store/src/artifact_referrer.rs:739-782,886-905`).

Each artifact store applies an end in one transaction:

1. Insert the ended referrer's permanent fence idempotently.
2. Apply carries before severing. Skip a fenced destination. Otherwise add
   its edge to the stored artifact, or refuse `CarryArtifactMissing`.
3. Delete all edges of the ended referrer in that store's namespace.
4. Reclaim affected artifacts only when no edge remains. SQLite also checks
   the blob's other roots before reclaiming the shared blob.

Reapplying a completed cleanup changes nothing. SQLite serializes the
write; PostgreSQL takes sorted referrer locks before sorted artifact locks.
Publish and acquire follow the same referrer-before-artifact order
(`crates/lash-sqlite-store/src/artifact_store.rs:286-380`,
`crates/lash-postgres-store/src/postgres/artifact_store.rs:192-200,359-472`).

Attachment cleanup carries no artifacts through the attachment port.
ADR 0124 requires the receiver's edge before delivery releases its source;
the relay calls `end_attachment_referrer` for attachment-holding kinds
(`crates/lash-core/src/runtime/artifact_cleanup.rs:552-557`).

#### 2.4 Where cleanup records live, and the two ways they arise

`artifact_cleanup_obligations` holds one serialized `ArtifactCleanup` per
referrer, with ADR 0109's due, claimed and stalled states. A row exists only
while cleanup is owed. PostgreSQL has one ledger table. SQLite has a core
table and a registry table, so prune can record an end in its own database;
ids have `core:` or `registry:` prefixes and settlement routes by prefix
(`crates/lash-sqlite-store/src/schema.rs:689-710,1372-1393`,
`crates/lash-sqlite-store/src/obligation_ledger.rs:538-631`,
`crates/lash-postgres-store/schema.sql:1091-1113`).

An end hook writes `Ended` beside the durable fact that ends the referrer.
An `Ended` plan replaces a guard; a guard cannot replace an existing plan.
When the end shares the artifact database, arming it also writes the fence
in that transaction. A first guarded acquisition writes its guard before
committing the edge. Guard resolution asks the relevant durable authority,
so a publication whose start or subscription never commits still has an
end path (`crates/lash-sqlite-store/src/obligation_ledger.rs:578-598`,
`crates/lash-store-sql/src/artifact/cleanup_obligations.rs:37-68`,
`crates/lash-sqlite-store/src/artifact_store.rs:209-218,276-281`).

#### 2.5 The one executor

`ArtifactCleanupRelay` uses `StoreSet::artifact_cleanup()` and the
`ArtifactCleanup` obligation kind. It is the executor that severs edges
outside an end hook's transaction
(`crates/lash-core-execution/src/backend.rs:204`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:190-215`). Its delivery:

1. Loads the cleanup; a row another relay settled is already delivered.
2. Defers while the optional gate journal may replay.
3. Resolves the plan. `Ended` supplies its carries; an ended start also
   protects any concurrently registered record before severing (§3.3).
   `AwaitJournal` waits for journal settlement. `AwaitStart` carries the
   retained record's inputs or waits for an absent start's journal to
   settle. `AwaitSubscriptionRevision` uses §3.4's three conditions.
   `AwaitFrame` keeps a retained frame and otherwise waits for its creator
   to settle. ADR 0124 owns upload-expiry and session-graph guards.
4. Applies each store's share to environments, modules, definitions and
   every installed engine. Attachment-holding kinds also end their
   attachment edges.
5. Answers success only after every store succeeds; the ledger then settles
   `Delivered` and deletes the obligation row. Fences remain permanent.

The resolution and delivery are in
`crates/lash-core/src/runtime/artifact_cleanup.rs:238-325,505-559,606-645`;
settlement is in
`crates/lash-store-sql/src/artifact/cleanup_obligations.rs:125-149`.
`NotYet` defers at the relay's maximum backoff with attempts reset.
`NotBefore` defers at the earlier of its known due instant and that backoff.
A missing carry stalls as refused, an undecodable row stalls as undecodable,
and store faults retry the delivery. A retry repeats every store
idempotently; partial success is never acknowledged
(`crates/lash-core-execution/src/runtime/drive/relay.rs:184-205`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:571-593`).

`EffectHost::journal_replay` returns `MayReplay` or `Settled`. `Settled`
promises that the journal cannot replay or append. Restate uses durable
facts and its invocation status:

- A turn needs a root terminal and no open run of that root.
- A queue drain needs no open session drive or root run.
- A process needs terminal evidence and no open segment run; a pruned
  process is settled.
- A runtime operation needs its durable waits retired after its commit.
- Session deletion runs no publication and is settled.

Wait retirement alone does not settle a turn or process journal
(`crates/lash-restate/src/effect_host/journal_verdict.rs:28-79,89-183`).

#### 2.6 Hosts

`LashCore::host_artifacts()` exposes `HostArtifacts`. A host mints an opaque
`HostArtifactPin`, publishes modules or environments under it, and uses
`publish_definition` or `pin_definition` to hold a definition's closure.
`get_definition` returns a snapshot and acquires no pin. `release` writes
the pin's `Ended` record and fence; cleanup severs its edges. A released
pin cannot publish again; another publication needs a fresh pin
(`crates/lash/src/artifacts.rs:72-165`,
`crates/lash-core-store/src/artifact_referrer.rs:443-483`).

#### 2.7 Typed errors

`ArtifactStoreError` distinguishes `ReferrerEnded`, `ArtifactMissing`,
`CarryArtifactMissing`, `Immutable`, stored corruption, incompatibility,
store refusal and backend failures, plus encode/decode failures. The
`StoreError` conversions preserve the referrer and artifact names.
`artifact_referrer_ended` classifies the typed refusal across the plugin
boundary (`crates/lash-core-execution/src/module_artifacts.rs:31-96`,
`crates/lash-core-execution/src/runtime/process/model.rs:271-310`).

### 3. End hooks

#### 3.1 `frame_environment`: frame commit, session deletion, fork

A cell publishes under its execution journal, then acquires its admitted
frame's edge. At cell end RLM acquires every tagged definition reachable
from the globals and its complete manifest. The published-module cache is
keyed by frame and module. Overwriting a global conservatively retains its
edges until the frame ends. Every definition in the frame's globals thus
has the frame's protection, the I-frame invariant
(`crates/lash-protocol-rlm/src/executor/mod.rs:184-194,938-1037`).

Every committed frame open clears execution state. Its seed determines
which definitions carry into the successor; bare module references and
copied digest strings carry none. All frame authors call the same seed
carry derivation, including `continue_as`, context-pressure hooks,
compaction and overflow recovery. The standard compaction plugin belongs
to the standard protocol; RLM's model-driven switch is `continue_as`
(`crates/lash-core-store/src/session_state.rs:1759-1763`,
`crates/lash-core/src/runtime/turn_boundary/execution_state.rs:41-104`,
`crates/lash-protocol-rlm/src/plugin/runtime_state.rs:414-449`,
`crates/lash-plugin-standard-compaction/src/lib.rs:1-16`).

Before SQL activates a successor, the parent validates carried definition
descriptors and prepares their engine-store manifest entries under the
successor's guarded frame claim. `AwaitFrame { creator }` protects an
aborted preparation until its creator cannot replay. It keeps a committed
frame retained by a head, anchor or admission root. Admission retention
conservatively protects its session's committed nodes until release
(`crates/lash-core/src/runtime/frame_definition_carry.rs:8-65`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:255-266`,
`crates/lash-postgres-store/src/postgres/session_sql.rs:403-415`).

Inside the head commit, each carried definition is decoded again and its
SQL-owned manifest entries are acquired under the successor. Every such
entry must have the source frame's edge; otherwise the commit refuses
`ArtifactCarryMissing`. No engine artifact bytes are copied
(`crates/lash-sqlite-store/src/persistence/session_commit.rs:27-83`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:100-164`).

A commit ends every frame it leaves: the prior head's frame and every frame
it appends, in graph order, except the frame its new head holds. The store
derives this chain in the commit transaction. `FrameTransition` names the
carry source, successor, carries and replay gate; its source must be one
of the frames the commit leaves. The transaction fences each left frame
and writes its empty-carry `Ended` record. A transition gates every end in
the chain; a commit without a transition ends the chain ungated
(`crates/lash-core-store/src/store/runtime_commit.rs:95-137`,
`crates/lash-sqlite-store/src/persistence/session_commit.rs:93-115,806-824`,
`crates/lash-postgres-store/src/postgres/runtime_persistence/session_commit.rs:6-74`).

Session deletion fences the current frame and records its ungated end in
the deletion transaction. A fork of a live frame copies the source's SQL
artifact edges under the fork's frame identity. A fork of a fenced frame
strips execution-state components from its checkpoint instead
(`crates/lash-sqlite-store/src/session_deletion.rs:132-142`,
`crates/lash-sqlite-store/src/forks.rs:332-357`,
`crates/lash-postgres-store/src/postgres/session_factory.rs:665-694,834-847,1073-1114`).

#### 3.2 `process_record`: engine settlement, registry prune

A registered start's cleanup carries its retained inputs to
`ProcessRecord(id)`. When a start meets a fenced key, registration acquires
the record's adopted inputs directly under the record. Terminal completion
retains those inputs; prune writes the record's empty-carry end beside its
tombstone. PostgreSQL also fences it in that transaction. The same cleanup
ends its attachment edges
(`crates/lash-core-execution/src/runtime/process/start_staging.rs:302-313,393-430`,
`crates/lash-sqlite-store/src/process_registry_change.rs:196-208`,
`crates/lash-postgres-store/src/postgres/prune.rs:44-66`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:552-557`).

#### 3.3 `start`: engine start and replay settlement

A start stages its environment, engine artifacts, and any definition
closure under `Start(key)` before registration. Existing or inherited
environments need an acquisition too. The first acquisition arms
`AwaitStart { starter }`; independent engine acquisitions arm it through
the ledger first. Once registration commits, the journaled step nudges
the guard. Resolution carries the retained record's inputs to its record,
including its descriptor and manifest for a start by definition id
(`crates/lash-core-execution/src/runtime/process/start_staging.rs:319-430,640-718`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:271-279,346-362,454-502`).

A terminal refusal with no registered record arms an empty-carry `Ended`
plan. An absent start is also ended when its starter's journal settles.
Because keys are global, resolving an `Ended` start reads the key again
*after the fence* and acquires any retained record's content under that
record before severing the start. This protects a registration racing the
refusal. Missing bytes in this rescue are skipped, since a start that met
the fence acquires its own content; a fenced record belongs to its own
cleanup. The refusing start itself writes no rescue edge
(`crates/lash-core-execution/src/runtime/process/start_staging.rs:348-376`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:250-252,365-451`).

#### 3.4 `subscription_revision`: subscription retirement, delivery settlement

`Register`, `Update`, `Revive`, and a state-changing `Enable` or `Disable`
acquire the committed revision's environment and target before the command
commits. `Delete` and `Prune` acquire no deliverable revision. The claim
arms `AwaitSubscriptionRevision { creator }`. After commit, the wrapper
nudges the replaced revision
(`crates/lash-core-execution/src/triggers/revision_referrer.rs:64-151,179-218`).

A revision ends only when it is not the current nontombstoned revision,
no delivery reserved under its incarnation/revision remains unbound, and
its creator's journal is settled. A disabled current revision still holds
its artifacts. Binding follows the delivery start's acquisition, so the
revision needs no carries
(`crates/lash-core/src/runtime/artifact_cleanup.rs:118-153,281-292`,
`crates/lash-core-execution/src/triggers/router.rs:624`,
`crates/lash-core-execution/src/runtime/process/start_staging.rs:302-313`).

#### 3.5 `host_pin`: host release

Host publication acquires the pin's edges. `HostArtifacts::release` arms
its empty-carry end and fence in one core transaction. An unreleased pin
keeps its edges indefinitely; release is eventual reclamation through the
relay, not synchronous reclamation
(`crates/lash/src/artifacts.rs:156-165`,
`crates/lash-sqlite-store/src/obligation_ledger.rs:595-598,726-739`).

#### 3.6 Definition artifacts: immutable, held by the six kinds

A definition is an immutable canonical descriptor of engine kind, engine
value and sorted, deduplicated artifact manifest, named by its
content-derived `ProcessDefinitionId` (ADR 0095). It is the collectible
artifact kind `ArtifactStoreId::ProcessDefinition`, not a referrer kind.
The descriptor has no name, revision or lifecycle. Hosts own any names and
versions; lash has no named definition registry or definition CAS
(`crates/lash-core-execution/src/runtime/process/definition.rs:81-151,189-252`,
`crates/lash-core-execution/src/runtime/process/definition_store.rs:4-25`).

Every reader holds the descriptor and complete manifest under the same
referrer. The engine checks the manifest and derives the signature before
publication or acquisition. Engine-owned entries are prepared first under
the claim, with a guard where it has one; the descriptor and SQL-owned
entries commit together. A usable result is exposed only after this closure
is held. Partial preparations stay under their referrer's end rule
(`crates/lash-core-execution/src/runtime/process/definition_store.rs:136-243`,
`crates/lash-sqlite-store/src/artifact_store.rs:384-461`,
`crates/lash-postgres-store/src/postgres/artifact_store.rs:255-356`).

Equal canonical content shares an id. Publication verifies the stored
bytes even on an existing id; conflicting bytes are `Immutable`. Reads
re-derive the id and reject noncanonical bytes. `ProcessDefinitionStore`
stores descriptors in the `process_definition` namespace beside modules
and environments. A start by id realizes an engine input and records that
id in the process identity. Frame globals, starts, records, revisions,
journals and host pins hold their own closure edges
(`crates/lash-core-execution/src/runtime/process/definition_store.rs:49-85,229-243`,
`crates/lash-core-execution/src/runtime/process/start_staging.rs:640-718`,
`crates/lash-sqlite-store/src/artifact_store.rs:102-113`,
`crates/lash/src/artifacts.rs:101-153`).

An id kept in a host table or copied as a string holds nothing. Between
starts, availability without another reader requires an explicit host pin.
Cleanup reclaims a descriptor and its dependencies after the last edge is
severed, subject to replay gates and carries. Shared modules remain while
another referrer holds them
(`crates/lash-core-execution/src/runtime/process/definition_store.rs:12-25,72-85`,
`crates/lash-sqlite-store/src/artifact_store.rs:46-53`,
`crates/lash/src/artifacts.rs:118-153`).

#### 3.7 `execution`: replay settlement

RLM publication and realized tool intents hold artifacts under the enclosing
journal's `Execution` referrer. The first acquisition arms `AwaitJournal`;
its end requires `journal_replay` to answer `Settled`. A terminal durable
fact alone cannot authorize cleanup while that journal may replay
(`crates/lash-protocol-rlm/src/executor/mod.rs:955-979`,
`crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:644-660`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:268-269`).

### 4. Ordering rules

#### 4.1 A frame switch racing publication

The switching commit fences the source frame. A replay can still publish
under its execution edge; RLM treats a fenced frame acquisition as an ended
frame and continues. The old frame's cleanup waits for the switching
journal, while prepared engine edges and atomically committed SQL edges
protect the successor. Severing at the switch would break post-commit
re-execution that reads the source's artifacts
(`crates/lash-protocol-rlm/src/executor/mod.rs:993-1006`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:630-645`, §3.1).

#### 4.2 A crash between publication and the referrer's record

A guarded acquisition durably records its cleanup before its edge commits.
Across stores the ledger guard is armed before engine acquisition. A crash
therefore leaves a referrer whose authority can settle its guard; it does
not require a host-driven drain or an inferred lifetime
(`crates/lash-sqlite-store/src/artifact_store.rs:276-281`,
`crates/lash-core-execution/src/runtime/process/start_staging.rs:168-178`).

#### 4.3 Key-coalesced starts

The registry chooses the retained record. Cleanup carries that record's
environment and engine artifacts, descriptor and manifest to its record,
then severs the shared start's edges. It does not carry a losing attempt's
inputs. Starts encountering the key's fence use the record path in §3.2;
terminal refusal races use §3.3's rescue acquisition
(`crates/lash-core/src/runtime/artifact_cleanup.rs:346-362,454-502`).

#### 4.4 A trigger delivery after its creator is gone

Revision edges precede visible registration. They remain independent of
the creator's frame or session and cover unbound deliveries. A delivery
acquires its own start protection before it binds; revision cleanup waits
for that binding rather than carrying the creator's references onward
(`crates/lash-core-execution/src/triggers/revision_referrer.rs:3-18`,
`crates/lash-core/src/runtime/artifact_cleanup.rs:118-153`).

#### 4.5 A cancelling child still reading its inputs

Cancellation does not end a process record's edges. The record holds its
inputs until prune, and its execution edges last until journal settlement.
The facade retires eligible process journals before deleting their records
(`crates/lash/src/process_admin.rs:774-803`, §3.2, §3.7).

#### 4.6 Cross-store prepare, acknowledge, activate and sever

SQLite's registry and trigger databases are separate from its core, and
an engine can own an independent artifact store. Protection therefore
precedes activation across stores: guarded starts and revisions acquire
before registration, and a frame prepares its engine share before SQL
activation. The descriptor and its store-set manifest share one transaction
(`crates/lash-core-execution/src/runtime/process/start_staging.rs:109-180`,
`crates/lash-core-execution/src/runtime/process/definition_store.rs:12-22`,
`crates/lash-core/src/runtime/frame_definition_carry.rs:8-65`).

Each store fences, carries, severs and reclaims in its own transaction;
the relay acknowledges only after every store succeeds. Pending cleanup
does not require blocking another switch: a carry into a fenced destination
is skipped, while the source stays held until its own cleanup applies.
A separate global transition transaction is unnecessary for the SQL-owned
frame share, which commits with the head
(`crates/lash-core/src/runtime/artifact_cleanup.rs:505-559`,
`crates/lash-sqlite-store/src/artifact_store.rs:357-377`, §3.1).

### 6. Creating a definition on this contract

`processes.create({ source, dialect })` is a leaf tool. Its attempt lowers
and links the source against the dispatch catalog and declares
`PublishDefinition` with the compiled module and descriptor. It publishes
nothing in the tool body. Realization publishes under the execution's
journal and returns the definition value with its derived signature.
The cell acquires its frame's closure edges before its result is exposed
(`crates/lash-lashlang-runtime/src/process_create_tool.rs:1-13,271-321`,
`crates/lash-core-execution/src/tool_dispatch/intent_executor.rs:621-624`,
`crates/lash-protocol-rlm/src/executor/mod.rs:184-194`).

Creating accepts no lifetime policy or name. A start acquires another
referrer and consumes nothing. A host has no frame and uses a pin; a frame
switch carries the definitions its seed names (§3.1, §3.3, §3.6).

### 7. Acceptance tests

The store matrix is SQLite file, SQLite memory and PostgreSQL. Host tiers
are the in-process Restate server double, live Restate and lash-sim's
in-process effect host; upgrade proofs use synthetic-next. Individual
registrations determine which laws run in each tier. Artifact-referrer
store laws are registered in
`crates/lash-sqlite-store/tests/conformance/suite.rs:540` and
`crates/lash-postgres-store/tests/conformance.rs:694`. The RLM evidence
fixture selects SQLite memory or PostgreSQL from
`LASH_POSTGRES_DATABASE_URL`, and requires the URL when
`LASH_REQUIRE_POSTGRES=1`
(`crates/lash/tests/artifact_referrers_evidence/fixture.rs:25-102`).

Definition store laws cover retention while any reader holds a descriptor,
reclamation after the last referrer, byte verification on an existing id,
and replay at six create/publication/start boundaries
(`crates/lash-conformance/src/conformance/process_definitions.rs:444,560,679-1035`).
Prepared-frame laws cover an aborted activation and retention after a
committed activation
(`crates/lash-core/src/runtime/artifact_cleanup_tests.rs:1088,1175`).

#### 7.1 Cold reopen

Cold-reopened globals keep their frame edges across turns
(`crates/lash/tests/artifact_referrers_evidence.rs:209`).

#### 7.2 Overwritten globals

Overwriting a global retains its edges until frame end
(`crates/lash/tests/artifact_referrers_evidence.rs:285`).

#### 7.3 Seed carries

`continue_as` carries only seeded definitions; a pressure seed uses the
same derivation
(`crates/lash/tests/artifact_referrers_evidence.rs:350,471`).

#### 7.4 Execution state at frame open

Frame opens clear live execution state
(`crates/lash-conformance/src/conformance/frame_open_redrive.rs:1412`).

#### 7.5 Publication racing frame end

Publication racing a frame end meets its fence
(`crates/lash-conformance/src/conformance/artifact_referrers.rs:50`).

#### 7.6 Abandoned starts

A start with no committed record ends after its starter settles
(`crates/lash-core/src/runtime/artifact_cleanup_tests.rs:886`).

#### 7.7 Acknowledgment across stores

A failed store acknowledgment prevents whole-delivery success and retries
(`crates/lash-core/src/runtime/artifact_cleanup_tests.rs:633`).

#### 7.8 Coalesced starts

A coalesced start carries the retained record's content
(`crates/lash-core/src/runtime/artifact_cleanup_tests.rs:657`).

#### 7.9 Subscription revisions

Revision cleanup waits for currency, bindings and its creator
(`crates/lash-core/src/runtime/artifact_cleanup_tests.rs:913`).

#### 7.10 Prune racing start rescue

Prune and a late start rescue respect fences
(`crates/lash-conformance/src/conformance/process_prune_start_staging.rs:15,294`).

#### 7.11 Journal gates

A replaying journal keeps its gate's artifacts
(`crates/lash-core/src/runtime/artifact_cleanup_tests.rs:618`).

#### 7.12 Definition closure

A carried definition id holds its closure, an uncarried definition ends,
and a host pin survives an uncarried switch
(`crates/lash/tests/artifact_referrers_evidence.rs:619,709,769`).

#### 7.13 Forks

Forks copy live-frame SQL edges or strip ended-frame execution state
(§3.1; `crates/lash-sqlite-store/src/forks.rs:332-357`).

#### 7.14 Host release

Host release reclaims and permanently fences the pin
(`crates/lash-conformance/src/conformance/artifact_referrers.rs:122`).

#### 7.15 Canonical encodings

Referrer ids have canonical encodings; unknown vocabulary is typed
incompatibility, and malformed ids are corruption
(`crates/lash-conformance/src/conformance/artifact_referrers.rs:356`, §1).

#### 7.16 Cleanup retry

Retrying cleanup after a destination ends is idempotent
(`crates/lash-conformance/src/conformance/artifact_referrers.rs:238`).

#### 7.17 Journal settlement

Restate answers settled only when nothing can replay the journal
(`crates/lash-restate-test/tests/journal_verdict.rs:31`).

## Consequences

Exact edges determine artifact availability; a definition id, process
completion or scope cancellation cannot infer its lifetime. Reclamation
is eventual because replay gates and durable cleanup obligations retain
source edges until the relevant authorities settle them (§2.5, §3).

Every ended referrer has a permanent fence. Host pins need explicit release;
overwritten globals retain their frame edges until frame end. Waiting
guards poll at the maximum backoff unless a nudge shortens it. Store faults
retry, and refused or undecodable obligations remain visible as stalled
work (§2.3-2.5, §3.1, §3.5) under
[ADR 0109](0109-store-to-engine-delivery-is-an-outbox-of-obligations.md).

Definition consumers each hold their own closure. A host maintains any
name/version mapping and a pin for availability between starts. Frame
switches clear execution state and retain only the definitions their seed
carries; forks of ended frames start without those execution components
(§2.6, §3.1, §3.6).
