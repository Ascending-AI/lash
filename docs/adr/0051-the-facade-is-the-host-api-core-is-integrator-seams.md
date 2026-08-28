# 0051. The facade is the host API; lash-core's public surface is its integrator seams

Date: 2026-08-03
Status: accepted, superseded in part by [ADR 0079](0079-one-promised-package-facade-owns-the-api.md)

ADR 0079 is authoritative for the single promised package and for replacing the
hand-maintained API example-coverage ledger. Its successor doctrine is:

> Every facade API is exercised by a compiled example or doctest. Enforced
> mechanically wherever compiler derivation reaches; the remainder is a
> review-time expectation recorded in this ADR — never a prose ledger.

The generated facade surface snapshot, semver baseline, external-type allowlist,
facade-only import scan, and missing-documentation checks are the successor
gates. The inventory and its checker are historical context below, not current
enforcement.

## Decision

The `lash` crate is the supported host API, in full. `lash-core` keeps a public
item exactly when a named integrator class needs it, and for no other reason.
Everything else in `lash-core` is crate-private or reachable only through
internal seams the facade consumes. Public surface is a promise we test and
keep; anything we are not prepared to promise is not public.

The named integrator classes, each seeded by the interfaces an implementor must
write against, are:

1. **Store and durable-substrate implementors** — the store traits
   (`RuntimePersistence`, `SessionStoreFactory`, `ProcessRegistry`,
   `TriggerStore`, `AttachmentStore`, `LiveReplayStore`,
   `ProcessExecutionEnvStore`, `ProcessContinuationStore`, and their sibling
   store contracts) and every type their signatures reach.
2. **Effect-host implementors** — `EffectHost`, `RuntimeEffectController`,
   `AwaitEventResolver`, and their signature closure.
3. **Protocol and process-engine implementors** — `ProtocolSessionPlugin`,
   `ProtocolDriverPlugin`, `CodeExecutorPlugin`, `ProcessEngine`, and the other
   engine extension points, with their closure.
4. **Conformance-suite embedders** — everything
   `lash::testing::conformance` exposes, closed over its signatures, so an
   integrator can hold a custom backend to the same executable contract the
   built-in backends answer to.

Membership is decided by **transitive signature closure**, not by direct-use
scanning. A type that appears only in a public trait method's parameters or
return type is integrator surface, whether or not any repository code names it
today: `ExecRequest` exists because `CodeExecutorPlugin::execute_code` exists.
The narrowing measurement found 683 items that a direct-use scan classified as
removable but that the closure proves are load-bearing for implementors; the
closure rule is therefore normative, and any future tooling that classifies
surface must apply it.

The closure decides **members**, not only types. A type enters the closure in a
direction: an implementor either *produces* it (it appears in return position,
so the implementor must construct one) or *consumes* it (it appears in argument
position, so the implementor must read one), and a type reached through a field
inherits its owner's direction. A member of a closure type is integrator
surface exactly when the direction it serves makes it load-bearing:

- **Produce-side** — the constructors and builders an implementor needs to
  return a value of the type, including on types with public fields, because a
  struct literal is not a promise we can extend. Direction is per class, not per
  type: `RuntimeCommit` is consume-side for a store, which is handed one by
  `SessionCommitStore::commit_runtime_state`, and produce-side for a conformance
  embedder, which assembles one to hold that store to the contract — so its
  builders are integrator surface.
- **Consume-side** — the accessors an implementor needs to read the value it
  was handed. On an opaque type they are the *only* interface, so every one of
  them is load-bearing: `ProcessEngineRunContext` is what
  `ProcessEngine::run` receives, so its accessors are the engine contract.
- **Neither** — a member that only projects a value the implementor produces,
  or only mutates state the runtime owns, serves no direction. It is not
  integrator surface even when it sits on a closure type.

Direct-use scanning is as non-normative here as it is for types, and more
dangerous: a member can have no caller in this repository and still be the one
thing an implementor must call. `TurnContext`'s prompt mutators remain on the
facade turn builder; `TurnContextTransform::transform` receives and returns a
`PreparedContext` and never sees `&mut TurnContext`. `set_prompt_layer` remains
inherent for `lash-remote-protocol`, while the other prompt mutators belong to
the facade seam.

Members that are not integrator surface get one of two homes, chosen by who
holds the receiver:

- `pub(crate)`, or a seam trait re-exported through `lash_core::facade_support`
  when the facade needs it across the crate boundary, for receivers only the
  runtime and the facade ever hold.
- A public `lash::<domain>::<Type>Ext` extension trait, example-covered from
  birth, for receivers a *host* holds where the behavior is host convenience
  the core contract does not need. Same-named trait methods keep existing call
  sites compiling, so the migration is an import, not a rewrite.

The package-version constants (`lash_core::VERSION`, `SANSIO_VERSION`) are not
integrator surface. Compatibility between an integrator and the runtime is
expressed through trait contracts, data shapes, and the schema-version
machinery — never by gating on a package version.

## Amendment: plugin authoring is facade surface (2026-08-23, FIG-1921)

Writing a plugin is not one of the four integrator classes, so a plugin crate
must be able to compile against `lash` alone. The dividing rule inside
`lash_core::plugin` is the closure rule applied to the plugin author as the
implementor: **an item is authoring surface, and therefore has a
`lash::plugins` home, when an in-tree example plugin or a downstream host
plugin names it to compile** — in a trait it implements, in a handler signature
it writes, or in a value it constructs or reads. Everything else in that module
serves the runtime embedding path and stays core-only under the classes above.

The re-exports are flat on `lash::plugins`, matching every other domain module
(`tools`, `persistence`, `observe`). The facade has exactly one prelude,
`lash::prelude`, for the daily core/session/turn vocabulary; a second,
domain-scoped prelude would give plugin authoring two obvious ways in.

FIG-1921 moved the plugin-operations vocabulary (`PluginOperation`,
`PluginQuery`, `PluginCommand`, `PluginTask`, `SessionParam`,
`PluginQueryContext`, `PluginCommandContext`, `PluginTaskContext`,
`SessionReadService`, `ProcessReadService`, `PluginOperationOutcome`,
`PluginRuntimeDirective`, `PluginOperationFailure`, `PluginOperationReceipt`,
`PluginOwned`, `PluginOperationInvokeError`) and the plugin snapshot seam
(`SnapshotWriter`, `SnapshotReader`, `PluginSnapshotMeta`,
`SessionReadyContext`) to `lash::plugins`. `PluginOperationReceipt` and
`PluginOperationInvokeError` were already in `lash`'s own signatures — on
`PluginOperations` and on `EmbedError::Control` — with no path a host could
name, which is the sharpest form the gap took.

One home each: `lash::admin` used to re-export `PluginQuery`, `PluginCommand`
and `PluginTask` so its operation runners' bounds were nameable. Those traits
are authoring surface, so `lash::plugins` is now their only home and `admin`
re-exports them no longer — a host satisfies the bound with its own type and
never writes the trait name to invoke an operation.

These plugin-namespace items are **integrator seams and stay core-only**:

- **Protocol and process-engine extension points** (class 3):
  `ProtocolSessionPlugin`, `ProtocolDriverPlugin`, `CodeExecutorPlugin`,
  `AssistantProseProjectorPlugin`, `ProtocolRuntimeContext`,
  `ProtocolSessionContext`, `ProtocolBeforeLlmCallContext`,
  `ProtocolLlmCallAction`, `ProtocolSessionMaterialization`,
  `ExecutionStateSnapshot`, `ExecutionStateComponentSnapshot`,
  `HydratedExecutionState`, `ProcessEngineContributionContext`.
- **Runtime embedding**: `RuntimeServices`, `SessionAuthorityContext`. A plugin
  is handed services; it never assembles the set.
- **Catalog assembly alias**: `ToolContractResolver` remains core-only. A
  catalog hook receives it only as a field of `ToolCatalogContext`, and the
  alias expands entirely through the facade-nameable
  `lash::tools::ToolContract`; plugin authors can call it without naming the
  alias.
- **Runtime-side turn composition**: `PrepareTurnRequest`, `TurnPreparation`,
  `TurnFinalization`, `CheckpointApplication`, `PluginAbort`. These are how the
  runtime drives the registered hooks, not what a hook receives.
- **Plugin-session internals**: `PluginOperationRegistrations`,
  `SessionContextOverlay`,
  `SessionPluginSource`, `SessionRelation`, `AgentFrameAssignment`,
  `AgentFrameId`, `AgentFrameReason`, `AgentFrameRecord`,
  `OpenAgentFrameRequest`, `OpenAgentFrameResult`,
  `SessionObservedProcessOutcome`, `SessionObservedProcessReceipt`.
- **The persisted snapshot aggregate**: `PluginSessionSnapshot`,
  `PluginSnapshotEntry`, `PluginSnapshotArtifact`. A plugin writes blobs and
  returns its own `PluginSnapshotMeta`; the collection those land in is the
  runtime's, and no plugin names it.

FIG-1929 closed the remaining authoring gap by exporting the three registered
hook arguments (`ToolCatalogContext`, `ToolResultProjectionContext`, and
`AssistantStreamFinishedContext`), the stream-finished reason it carries
(`AssistantStreamFinishReason`), and the operation definition returned by
`PluginSession::plugin_operations()` (`PluginOperationDef` and
`PluginOperationKind`). `ToolCatalogContext`'s readable field closure —
`SessionToolAccess`, `SubagentSessionContext`, and `PluginExtensions` — follows
it onto `lash::plugins`; exposing those values a plugin is handed does not
expose the runtime-only services or session-authority assembly path.

The rule is enforced, not asserted:
`facade_only_plugin_authoring::example_plugins_need_no_lash_core_import` in
`examples/docs-snippets` fails when any in-tree example plugin module names
`lash_core` in code. A plugin type that cannot be reached from `lash` is
therefore a facade gap the next such module discovers, not a carve-out.

### Read-only handles

Inspection hosts read settled session history, tree, and usage through
`LashCore::read_session`, which returns the same `SessionReadView` a live
session exposes without opening a runtime or acquiring its lease. Store
implementors provide that capability through `SessionStoreFactory::read_session`;
SQLite's `SqliteSessionStoreFactory::open_read_only` opens the catalog with
`mode=ro` and never exposes its internal persistence handle. This prevents the
read path from mutating durable session, lease, claim, or graph state. It is not
a filesystem no-write guarantee: when no live connection has materialized a
WAL catalog's wal-index, SQLite may create the catalog's `-wal` and `-shm`
sidecars while reading it. A catalog on read-only media therefore cannot be
inspected unless the required sidecars already exist; that SQLite failure is
reported as a backend error. The reader does not use `immutable=1`, which would
be unsound while another process may hold a writer.

## Why

Three defects in one week were unused-public-surface defects: a drain API with
zero callers concealed a wake-mark over-claim; an append precondition with one
since-deleted caller was read in opposite ways by two capable reviewers; an
orphaned retention lever meant deleting an unsafe deletion left a table with no
reclamation path. Surface nobody consumes drifts until its first consumer
discovers what it actually does. The example-coverage rule (the inventory and
its CI contract) makes such drift visible, but visibility alone would have us
writing examples against 5,424 public core items — most of which exist only
because the facade's implementation happens to live in another crate. Narrowing
visibility deletes no functionality: hosts keep everything through the facade.
It deletes *promises we never meant to make*.

The measurement behind this decision classified every public `lash-core` item:
2,848 are reachable by no integrator class (942 of them by nothing at all,
including the facade); 2,574 are integrator surface under the closure; 2 were
contested and are resolved above. The facade's own 6,821 items produced zero
trim candidates — the facade is already the deliberate API this ADR makes
authoritative.

## Consequences

- Anything consuming `lash-core` directly that is not one of the four classes
  is unsupported. The measured exceptions — 79 items held only by direct
  example or downstream-host use — are treated as **facade gaps**: each gets a
  facade home (or the consumer moves to an existing one) rather than a
  perpetual carve-out.
- The example-coverage inventory shrinks with the surface, and the coverage
  end-state ("every public API used and tested through our examples") is now a
  statement about surface we deliberately promise. Conformance suites remain
  the integrator classes' executable contract, and integrator-facing surface
  is exercised by integrator-class examples rather than shoehorned into host
  examples.
- Internalization proceeds in waves: first the zero-consumer items, then the
  facade-gap moves, then the remaining facade-only items. Each wave is
  breaking for direct `lash-core` consumers and carries `Breaking:` release
  notes naming the facade or seam replacement.
- New public items in `lash-core` must name their integrator class in rustdoc.
  An item that cannot name one belongs behind the facade.
- FIG-863's measured facade-seam floor is 3,193 `lash-core` rows, replacing
  wave D's 3,050–3,150 forecast; the count is an outcome of applying the rule,
  not a goal. Measured against the member-level closure above,
  455 of the 561 inherent members on retained `lash-core` root exports have a
  proven caller outside `lash-core`, and most of the remainder are load-bearing
  in a direction no in-repo crate exercises yet. A wave that moved them to hit
  a number would delete integrator ergonomics, not unkept promises. The core
  row count is an outcome of applying the rule, never the input.
- `lash-remote-protocol` converts wire DTOs to and from core types and cannot
  depend on the facade, because the facade depends on it. Its `core-conversions`
  feature is therefore a fifth seeding point for the closure alongside the four
  classes. It reaches 70 inherent members on retained root exports, 52 of them
  as the sole caller outside `lash-core`; all are retained core surface.
- The example-coverage inventory keys one row per **API item**, not per path
  (FIG-955), so "a `lash-core` row" now means an item with no facade projection.
  At that keying the surface is 7,516 items, of which **826** are reachable only
  through `lash_core`. The 3,193-row facade-seam floor above counted paths, and
  that path count has since grown to 4,233 (FIG-863 measured 3,193; #258 added
  949 reachability-closure rows and #244 one more). Of those paths, 3,407 name
  items the facade already re-exports, and each was carrying a second — often
  contradictory — disposition for the same contract. The rule is unchanged and
  the count remains an outcome rather than a goal, but a path count and an item
  count measure different things and must not be compared.
- **Per-path existence stays enforced.** Retiring a `lash_core::` re-export is
  breaking per the internalization bullet above, and item keying would have made
  it invisible (the retired path is an alias, not a row). So each row records its
  item's remaining public paths in `aliases`, derived from rustdoc the same way
  `availability` and `kind` are, and any path appearing or disappearing fails
  `scripts/check_api_example_coverage.py`. Only the *disposition* is centralized
  on the item; the path set is not, and a wave that internalizes core paths must
  still edit the inventory row by row.
- **`#[doc(hidden)]` is not a ledger exemption.** Hiding the internal
  cross-crate support modules from host-facing rustdoc is a documentation
  choice; whether the inventory answers for their paths is a separate API
  question, and one switch for both turned `lash_core::facade_support` into an
  amnesty channel — 304 paths no row covered, and 126 items whose written
  `unused-remove` verdicts were discharged by moving them there (FIG-1223).
  The checker documents hidden items, records the gated support modules in the
  inventory, tiers every anchor by its path shape (an example's host code, an
  example's tests, another crate's `src/`, a `tests/` directory) and validates
  each disposition against the tiers it may anchor in. Internal seams carry
  `internal-consumed` — justified by an anchor in a *consuming* crate's `src/`,
  checked on every run rather than asserted in prose — or `internal-test-only`
  when nothing but tests reach them. Every `unused-remove` row leaves a
  `[[removal_verdict]]` tombstone: a removal verdict is discharged by removing
  the item, never by relocating it, and a path that reappears elsewhere needs an
  explicit superseding disposition in the same diff.
- **Machine-verified evidence is verified against the item, not against a
  string.** A tier follows the code's compilation, so a file a parent declares for
  tests is test code even though the file itself shows no marker — whatever the
  `cfg` predicate says and wherever `#[path]` sends it. A member's anchor must tie
  its line to the type that owns the member: qualified on the line, or reached
  through a receiver that resolves — field by field, method by method, through
  `type` aliases, variant payloads and `impl Trait for Type`, and assembled across
  the continuation lines a fluent chain is written on — to that type or to the
  trait that owns the member. A field written in a literal is judged by the
  literal it sits in, because adjacent literals write the same field name for
  different types, and an anchor inside a type declaration is no anchor at all: a
  crate declaring its own same-named field is not consuming ours, and neither is
  the crate whose source declares the item — which the ledger's path root does not
  reveal, since `lash_core::PreparedTurnMachine` is declared in `lash-sansio`. Two
  anchor shapes are ruled explicitly: an **import is not consumption** (a `use`
  resolves whether or not anything needs the item, single-line or spread down a
  brace list), while a **trait-impl signature is** (implementing the contract is
  the strongest form the dependency claim takes). A bare
  occurrence of the name — no qualification, no receiver, no literal, no
  implementation — is a coincidence, not evidence. Failing closed on evidence is
  right; failing closed into a *deletion instruction* is not, so a row any earlier
  round tied to a consumer — by anchor or in prose — keeps that candidate in prose
  for a reader instead of acquiring a removal verdict.
  Naming no rival is not a defence: prelude and file-local types cannot be named,
  so a receiver nobody can follow to the owner fails on its own. The same
  predicate governs the *search* for a consumer, not just the check on an anchor
  already written — a name-based search answers a different question and reports
  live API as dead. A leaf name matches by coincidence — `as_str` on a
  `serde_json::Value` once justified an internal seam — and prose citing a line
  that never mentions the item is the same failure spelled in words. An item whose
  consumer cannot be established that way carries a removal verdict for a reader
  to confirm, not a justification nobody checked.
- No justification may park a `lash-core` (or other facade-dependency) consumer
  behind a pending migration to the facade. The cycle this ADR names for
  `lash-remote-protocol` holds for every crate the facade is built on. The
  checker derives that crate set from the resolved dependency graph
  (`cargo metadata`) and reads the claim per sentence, so stating that a caller
  *cannot* migrate — the honest description of the cycle — is not flagged.
