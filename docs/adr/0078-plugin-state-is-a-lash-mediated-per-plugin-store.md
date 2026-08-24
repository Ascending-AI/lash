# Plugin state is a lash-mediated per-plugin store

## Status

Accepted. Ratified on FIG-2006, which also binds the six invariants this ADR
implements as an interface. Implemented separately by the FIG-2006 cutover
children.

## Context

A `SessionPlugin` today owns its state privately and *asserts* its freshness.
The trait carries `snapshot(writer) -> PluginSnapshotMeta`, `snapshot_revision()
-> u64`, and `restore(meta, reader)`; lash captures the snapshot at turn
boundaries and gates recapture on a fingerprint hashing `(plugin_id,
plugin_version, snapshot_revision)`. Everything about that arrangement is
downstream of one fact: plugin state lives behind `Arc<dyn SessionPlugin>` with
interior mutability, so **lash never sees a mutation happen** and has to ask the
plugin whether one did.

The consequences are not hypothetical. No in-tree plugin overrides
`snapshot_revision`, so the fingerprint is a constant: capture runs once per
session and never again. A failed capture is stamped fresh and never retried. A
stamp can be written for a body that was never stored. Each of those is a
separate defect and all three have the same root — an unowned freshness
assertion.

The prospect round (FIG-2006's reference briefs: VS Code `Memento`, pi's harness
session values, salsa's revision protocol, Zellij's plugin lifecycle) converged
on one answer. Salsa states the general rule most precisely: the guarantee comes
from owning *both* sides of the protocol — a field that cannot be mutated except
through a framework setter, and reads that record what they read. Salsa also
names the boundary of that guarantee: an input holding an `Arc<Mutex<T>>` can
still be changed behind salsa's back, because the framework owns the field, not
the object graph reachable from its value. VS Code demonstrates the same shape
at extension scale: an extension mutates host-owned JSON through `Memento`, and
the host owns scoping, serialization, and persistence — with no revision hook,
no snapshot callback, and no flush API anywhere on the extension surface.
Zellij, from the other direction, shows the cost of not mediating: plugin state
is ordinary WASI file I/O, freshness is unobservable to the host, and
resurrection restores a layout rather than a plugin.

So the fix is not a better assertion. It is removing the plugin's ability to
make one.

## Decision

**A plugin's durable session state is a lash-owned, per-`(session, plugin)`
key-value store of JSON values. Plugins read and write it only through the host
API. Lash owns the generation, the durability boundary, the fork copy, and the
checkpoint component.**

The six invariants ratified on FIG-2006 are binding and are restated where each
one lands below. What this ADR adds is the exact interface.

### 1. The surface

One handle type, delivered by lash, bound at construction to exactly one
`(session, plugin)` pair. It is `Clone + Send + Sync + 'static`, and it holds no
plugin state of its own — it is a capability, not a container.

```rust
pub struct PluginStateStore { /* opaque; host-owned */ }

impl PluginStateStore {
    pub fn session_id(&self) -> &str;
    pub fn plugin_id(&self) -> &str;
    pub fn generation(&self) -> u64;

    pub fn get(&self, key: &str) -> Option<serde_json::Value>;
    pub fn get_as<T: serde::de::DeserializeOwned>(
        &self,
        key: &str,
    ) -> Result<Option<T>, PluginStateError>;
    pub fn keys(&self) -> Vec<String>;

    pub fn set(&self, key: &str, value: serde_json::Value) -> Result<u64, PluginStateError>;
    pub fn set_as<T: serde::Serialize>(
        &self,
        key: &str,
        value: &T,
    ) -> Result<u64, PluginStateError>;
    pub fn remove(&self, key: &str) -> Result<u64, PluginStateError>;

    pub fn apply(
        &self,
        edits: Vec<PluginStateEdit>,
    ) -> Result<u64, PluginStateError>;
    pub fn apply_guarded(
        &self,
        expected_generation: u64,
        edits: Vec<PluginStateEdit>,
    ) -> Result<u64, PluginStateError>;
}

pub enum PluginStateEdit {
    Set { key: String, value: serde_json::Value },
    Remove { key: String },
}
```

**Everything is synchronous.** No method on this surface performs I/O: a read is
a map lookup under a host mutex, a write is a map update plus a counter bump.
Making it `async` would be a lie about what it does, and it would force `.await`
into `register`, which is not a future. The durable write happens later, at the
boundary commit, driven by lash — section 5.

**Mutating calls return the new generation.** That is the plugin's acceptance
token: an ordinary value it can log, assert on, or feed to `apply_guarded`.

**`get` returns an owned `Value`.** Not a reference, not a guard, not an entry
API. This is the salsa caveat made structural, and it is the invariant an author
drifts on first, so the prohibition is explicit and complete:

- no `&mut Value`, no `Entry`, no `MutexGuard`, no `Ref`/`RefMut` returned;
- no `update_with(key, |value: &mut Value| ...)` read-modify-write closure, and
  no `Rc<RefCell<_>>` or `Arc<Mutex<_>>` handed across the boundary in a value;
- no `Deref` to interior storage and no borrow of the store's map.

A plugin that wants read-modify-write does `get` → mutate its own copy → `set`,
or `apply_guarded` when the interleaving matters. Every byte lash will ever
commit passed through a host call that bumped the generation.

The handle itself may be freely cloned and captured — including into hook
closures, which is how it reaches hook bodies (section 4). That is not an escape
hatch: cloning a handle copies no state, dropping one changes no state, and
every operation reachable through it is a mediated host call.

**`generation()` is not a durability signal.** It reports mediated writes, not
commits. There is deliberately **no** `committed_generation()`, no
`is_durable()`, and no notification when a boundary commit lands: each of those
is a `flush()` wearing a disguise, and the first plugin to poll one has
reinvented the mid-turn durable write that invariant 5 forbids. A plugin that
needs to act at a durability boundary registers a checkpoint hook, which exists
for exactly that.

### 2. Keys, values, and the error type

```rust
pub enum PluginStateError {
    InvalidKey { key: String, reason: KeyRejection },
    ValueTooLarge { key: String, bytes: usize, limit: usize },
    StoreTooLarge { bytes: usize, limit: usize },
    Encode { key: String, source: serde_json::Error },
    Decode { key: String, source: serde_json::Error },
    GenerationConflict { expected: u64, actual: u64 },
}
```

`PluginStateError: Into<PluginError>`, so a hook body uses `?` unchanged.

**Key rules.** A key is 1..=128 bytes of `[A-Za-z0-9._-]`. Rejections are
`Empty`, `TooLong`, and `IllegalCharacter { at, byte }`. Keys are not a
hierarchy: the plugin id already namespaces the store, so `/` buys nothing and
costs a path-like reading of a flat map. Keys appear in checkpoint bodies, in
behavior transcripts, and in error text, so they stay greppable and stable by
construction rather than by convention.

**Value rules.** Values are `serde_json::Value`. A single value is capped at
**32 KiB** of compact JSON; a `(session, plugin)` store is capped at **128 KiB**
of compact JSON across all keys. Both are hard rejections at the call, not
warnings.

VS Code caps nothing here — it warns at 512 KiB and points the extension at
`storageUri` files instead. That is right for an editor and wrong for lash, for
two reasons. A lash plugin store is re-encoded into a checkpoint component and
paid at the boundary commit, against a commit budget that is explicit host
policy ([ADR 0058](0058-runtime-commit-budgets-are-explicit-host-policy.md)), so
an oversized value is not a one-time cost — it is a tax on every boundary for
the life of the session. And lash offers no `storageUri` escape hatch, by
design: a host-owned file store for plugins would be a new durable artifact
class owing an owner and a reclaim trigger
([ADR 0067](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md)). State
that does not fit belongs in the *host's* own storage, keyed by session id,
where the host already owns its lifecycle.

The limits are constants in lash, not host configuration. A per-host knob would
make "does this plugin work" a deployment property; changing the numbers is an
amendment to this ADR.

**Every failure is deterministic.** No variant is transient, none is an I/O
error, and none depends on timing. A call is rejected as a function of its
arguments and the current store contents, so a plugin that validates once
validates forever. This is a load-bearing property: it means a plugin never has
to write retry logic against its own state, and a fixture can enumerate the
failure space exhaustively.

**Batches are all-or-nothing.** If any edit in `apply`/`apply_guarded` is
rejected, no edit is applied and the generation does not move.

### 3. Generation and batching

Per invariant 2, each `(session, plugin)` carries a monotonic `generation: u64`,
starting at 0, owned by lash. The rule is one sentence:

> **Every accepted mutating call bumps the generation exactly once, regardless of
> how many keys it touched.**

So N `set` calls in one hook are N bumps; one `apply` carrying N edits is one
bump. Batching is therefore a real, explicit tool with a visible effect, and the
plugin author who cares chooses it — rather than lash guessing at intent by
coalescing per hook, per turn, or per tick. A hook-scoped coalescing window was
rejected: it makes the generation a function of *where* the write happened
rather than *that* it happened, and it silently breaks `apply_guarded`'s
precondition.

Two edges, both ruled:

- **`set` never compares values.** Writing an identical value bumps. This is
  salsa's conservatism, adopted deliberately: value equality on arbitrary JSON
  is a cost paid on every write to avoid a cost paid at most once per boundary,
  and the content-addressed component (section 6) already collapses the
  identical-bytes case to an unchanged reference. A spurious bump costs one
  re-encode and yields the same `BlobRef`, the same descriptor, and the same
  commit identity.
- **`remove` of an absent key is a no-op** and does not bump, returning the
  unchanged generation. This is a membership check, not a value comparison — it
  is free, exact, and prevents a plugin's idempotent cleanup path from forcing a
  recapture every turn.

Generations are meaningful only within one `(session, plugin)`. Comparing them
across plugins or across sessions means nothing, except along a fork lineage
(section 7), where the child inherits the counter.

### 4. Where the store is exposed

Identity is bound at construction, never passed as an argument. No API anywhere
takes a `plugin_id` and returns a store, so a plugin **cannot name another
plugin's namespace** — invariant 3 holds by construction rather than by check.

Two delivery points:

- **`PluginRegistrar::state(&self) -> PluginStateStore`**, available during
  `register`. The registrar is already per-`(session, plugin)`; it is where a
  plugin builds its hooks, and the handle it hands out is captured into those
  closures like any other resource.
- **`SessionReadyContext { session_id, host, state: PluginStateStore }`**, so
  the plugin can read its hydrated state at the point invariant 4 names.

Hook contexts gain **no new field**. A hook body reaches the store through the
handle its closure captured at registration. The alternative — a
`plugin_state()` accessor on each of the fifteen hook context structs — was
rejected twice over: those contexts are constructed once and passed to every
plugin's hook, so the accessor would need a `plugin_id` argument and would hand
every plugin a forgeable route into every other plugin's namespace; and it would
spread one concept across fifteen structs to deliver a value the closure already
has.

**Ordering.** The store is hydrated from the checkpoint *before* `session_ready`
runs. The full sequence is: factory `build` → `register` → session construction
→ **store hydration** → `session_ready`. So `session_ready` is the first point
at which a plugin observes durable state, and it is also the point at which the
old `restore` callback used to run — which is why deleting `restore` costs
nothing (invariant 6): restore *is* reading the store on rebuild.

Writes are accepted from any of these points, including `register` and
`session_ready`, and from plugin operations and tasks running off the turn loop.

### 5. Read-your-writes, and the durability boundary

Precisely, in five clauses:

1. Each call is atomic. A batch is atomic. There is no multi-call transaction
   and none is offered.
2. Once a mutating call returns `Ok`, every later `get` for that
   `(session, plugin)` — through any handle, from any hook, from any task —
   observes it. There is no write-behind buffer, no per-hook staging area, and
   no per-turn overlay. Handles are views of one store, not copies of it.
3. Reads observe the in-memory store, which may be **ahead of** the last
   committed checkpoint. Per invariant 5, a write is accepted in memory at any
   time and becomes durable at the next boundary commit. There is no mid-turn
   durable write path and no method that requests one.
4. **Plugin state is not transactional with the turn.** A turn that fails after
   writing does not roll the write back; the value stays and commits at the next
   boundary. A plugin whose state must agree with a turn's outcome writes it
   from a hook on the committed path (turn-persisted or checkpoint), not
   mid-turn.
5. A session rebuilt from the store observes the last *committed* state; the
   uncommitted tail is gone. That is the same rule the rest of the session
   obeys, and it is the honest counterpart to clause 3 — the loss window is
   between acceptance and the next boundary commit.

VS Code's brief recommended returning an explicit accepted-versus-committed
signal, and we adopt the distinction while rejecting its second half: the
returned generation *is* the acceptance token, and lash deliberately publishes
no durability observation to plugins (section 1).

### 6. The checkpoint component

One component replaces the entire snapshot machinery.

- **Key** `plugin_state` (`PLUGIN_STATE_CHECKPOINT_COMPONENT`), one entry in the
  keyed component set of
  [ADR 0056](0056-checkpoint-components-generalize-to-a-keyed-set.md), carrying
  the current component encoding version.
- **Body**: a deterministic, canonically ordered encoding of
  `plugin_id -> { generation: u64, values: { key -> Value } }`, ordered maps
  throughout, so identical content encodes to identical bytes.
- **Identity**: content-addressed exactly like every other component under
  [ADR 0048](0048-checkpoint-component-identity-is-a-backend-contract.md) — a
  body mints its ref, a ref without a body means unchanged, an unknown ref is an
  error.
- **The generation lives in the body, not in the manifest.** This is the
  tool-state pattern verbatim: the resident typed view carries the generation it
  was decoded from, and nothing about plugin state earns a dedicated column on
  the checkpoint root. `SessionCheckpoint::plugin_snapshot_revision` is deleted
  rather than renamed.

**The gate.** At a boundary the runtime recaptures the component if and only if
the component is absent while some namespace exists, **or** some live namespace
generation differs from the generation carried by the resident component.
Otherwise it emits the unchanged reference. That is the tool-registry gate, and
it is now trustworthy for plugins for the same reason it is trustworthy for
tools: lash owns both ends.

Two of FIG-2006's three defects stop being reachable rather than being fixed.
Capture is a pure read of an in-memory map, so there is no failure to stamp
freshness from; and the generation is only advanced by the capture that
succeeded, so there is no stamp without a body.

Per-key generations were considered and rejected. Salsa's brief is explicit that
per-field tracking pays off only for partial invalidation, and this component is
captured whole; a per-key generation would be bookkeeping with no consumer.

### 7. Fork

Per invariant 3, a fork — subagent, child session, compaction child — starts
with a **deep copy of the parent's live store at fork time**, generations
included, and its own resident component marked absent so its first boundary
commits.

The copy is **verbatim and complete**: every namespace is copied, including
namespaces belonging to plugins that are not resident in the child. Residency is
a property of how a session was built; identity is the plugin id. Dropping
non-resident namespaces would make a plugin's state depend on which *other*
plugins the child happened to construct, and would silently erase the state of a
plugin that is absent from one build and present in the next. The stated cost:
state for a plugin that is never built again persists for the life of the
session and is reclaimed with it.

Content addressing makes the copy cheap in the store — the child's first body
hashes to the parent's ref whenever nothing has changed, so a fork adds a
manifest entry, not a blob.

Forks do not stay coupled. The parent's later writes are invisible to an
already-forked child, and vice versa; there is no merge and none is offered.

## Alternatives considered

**VS Code `Memento`'s dual global/workspace scoping — rejected.** Lash keeps one
scope: per-session. A cross-session ("global") plugin store would be a new
durable row class with no session to own it and no reclaim trigger to end it,
which [ADR 0067](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md)
exists to prevent. Host-wide plugin configuration is host-supplied data and
already has a home.

**`Memento`'s three-operation surface (`keys`/`get`/`update`) — adopted**, with
`update(key, undefined)` split into an explicit `remove`, and with `set`
returning a generation instead of a promise. Deleting by writing a sentinel is
cute in TypeScript and unnecessary in Rust.

**`Memento`'s async `update` — rejected.** Its promise resolves on RPC
acknowledgement, which VS Code's own storage path documents as "accepted into
host cache", not committed. Lash has no RPC hop and no cache, so an async
signature would carry the ambiguity without the excuse.

**`Memento`'s 512 KiB soft warning plus a file-store escape hatch — rejected**
in favour of hard caps and no escape hatch; see section 2.

**pi's typed values (`value<T>(namespace, key)`) — adopted in reduced form** as
`get_as`/`set_as`, defined entirely in terms of the JSON core so that invariant
4 holds and the durable shape stays inspectable. pi's own values doc leaves
limits, fork policy, and migration to the consumer; this ADR rules all three
(sections 2, 7, and Consequences), because lash is the consumer.

**pi's append-only lists (`list<T>`) — rejected.** A list primitive is a second
write algebra, a second durable shape, and a second reclaim question, to express
what a JSON array under the size cap already expresses. Growth that outgrows the
cap is a signal that the data belongs in host storage, not that lash needs a
log.

**pi's coding-agent custom session entries (state as transcript records) —
rejected.** It makes plugin state part of the conversation record and requires
the plugin to reduce its own history on every start. The lash store holds
continuation state, not an audit trail, and re-deriving state by scanning
entries is precisely the freshness ambiguity being removed.

**salsa's framework-owned write stamp — adopted** as the per-`(session, plugin)`
generation, including its conservatism about equal writes (section 3).

**salsa's per-field dependency tracking — rejected**; see section 6.

**salsa's `Arc<Mutex<T>>` caveat — adopted as a prohibition**, spelled out in
section 1. It is the one thing that would quietly reintroduce the defect this
ADR removes.

**Zellij's plugin-owned filesystem scopes (`/data`, `/cache`) — rejected.**
Unobservable freshness, unowned lifecycle, and no host-coordinated durability —
the model FIG-2006 is walking away from.

**Unconditional capture every boundary with content-addressed dedupe (the lens
brief's first-ranked option) — rejected as the ruling, retained as the safety
net.** It fixes freshness by never trusting it, at the cost of serializing every
plugin's state at every boundary whether or not anything changed. Mediation
gives the same correctness *and* the skip, and its dedupe still backstops the
equal-write case.

**Keeping `snapshot`/`restore` alongside the store — rejected.** Two ways to
persist plugin state is two freshness stories, and the weaker one decides.
Invariant 6 is a wholesale deletion for that reason.

## Consequences

- `SessionPlugin` loses `snapshot`, `snapshot_revision`, and `restore`. The
  trait becomes `id`, `version`, `register`, and `session_ready`.
- Deleted outright: `SnapshotWriter`, `SnapshotReader`, `PluginSnapshotMeta`,
  `PluginSessionSnapshot`, snapshot artifacts, the revision fingerprint, the
  `plugin_snapshot` checkpoint component, and
  `SessionCheckpoint::plugin_snapshot_revision` with its projections across the
  in-memory, SQLite, and PostgreSQL backends.
- Binary artifacts are gone with no replacement. Nothing in tree used them: the
  only implementor that ever overrode `snapshot` is the test `MockPlugin`, and
  the only non-empty bodies in the tree are framework fixtures. `MockPlugin` is
  rewritten as the store's fixture and remains the conformance witness.
- `version()` survives on the trait for diagnostics and stays out of the store
  key. Per invariant 3, an upgraded plugin sees the same namespace and owns its
  own value-schema migration — the same trade VS Code makes, and for the same
  reason: version-partitioned state is state the new version cannot find.
- Removing one Lash-owned component and adding another is an in-scope change to
  the projection governed by
  [ADR 0077](0077-session-state-migrates-totally-at-admission.md), so it bumps
  `session_state_version` — and **registers no converter step for the
  pre-cutover version**. A database written before this cutover therefore has no
  complete chain to the current version and **refuses at admission**, loudly and
  without mutation, exactly as ADR 0077 specifies for an unknown source version.
  That is the ruling: store version bumped, pre-cutover databases fail fast, no
  migration.

  Fail-fast costs nothing here, and it is worth saying why rather than leaving
  it as an assertion. No shipped plugin has ever written snapshot state — the
  only implementor that overrode `snapshot` is the test `MockPlugin` — so the
  data a converter would carry forward is empty in practice, and the old
  envelope's binary artifacts have no total representation as JSON anyway. A
  converter mapping every pre-cutover snapshot to an empty store would be a step
  that preserves nothing while keeping a pre-cutover database admissible, which
  sibling cutovers landing in the same store-version window refuse regardless.
  Registering it would add a chain member with no cargo and contradict the
  refusal those siblings already require.
- The plugin-state component is a typed component in behavior transcripts, per
  the expect-transcript doctrine — asserted as decoded content, never as an
  opaque blob size. Per-plugin generations join the session observation surface
  beside the tool-state generation.
- A plugin can no longer keep durable state lash cannot see. In-process caches
  and derived indexes remain the plugin's business; what changes is that they
  are now unambiguously *derived*, rebuilt in `session_ready` from the store,
  and never a thing lash is asked to persist.
