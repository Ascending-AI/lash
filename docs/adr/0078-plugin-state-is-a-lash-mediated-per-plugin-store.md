# Plugin state is a lash-mediated per-plugin store

## Context

Durable plugin state needs observable writes and a runtime-owned checkpoint
boundary. A plugin's private freshness assertion cannot prove that the state
captured at a boundary includes every accepted mutation.

## Decision

Lash owns a JSON key-value namespace for each runtime owner and plugin id.
For a session plugin the owner is its session. `PluginStateStore` binds that
identity once; reads and writes do not accept another plugin id. Plugin version
is diagnostic information, rather than a partition of durable state.

### 1. The surface

The opaque, cloneable handle exposes `owner`, `plugin_id`, `generation`, `get`,
`get_as`, `keys`, `set`, `set_as`, `remove`, `apply`, and `apply_guarded`.
Calls are synchronous map operations under a shared mutex; they perform no I/O.
Reads return owned JSON values. No mutable borrow, entry API, mutation closure,
or storage guard crosses the plugin boundary. A read-modify-write uses an owned
copy and a mediated write, with a generation guard when interleaving matters.

Generation is an acceptance token, not a durability signal. The handle has no
flush method, committed-generation accessor, or durability notification.

### 2. Keys, values, and the error type

Keys are 1 through 128 bytes of ASCII letters, digits, dot, underscore, or
hyphen. Namespaces are flat. A JSON value is capped at 32 KiB of compact JSON;
the namespace's values map is capped at 128 KiB. Limits are runtime constants.
Invalid keys, size excesses, typed encode/decode failures, and generation
conflicts return `PluginStateError`. Its `Into<PluginError>` conversion returns
`PluginError::State` with the typed variant and fields, so hook bodies use `?`.
`PluginStateError` and `KeyRejection` are cloneable and serializable; plugin JSON
and process journals retain their variants and fields. `Encode` and `Decode`
retain the key and diagnostic text in their `message` field.

Key, quota, and codec refusals are terminal for the same input. A generation
conflict requires reading current state and choosing a new edit; it carries
neither a terminal signal nor permission to retry the identical edit. The facade
preserves these classifications through plugin hook errors.

Batches validate a candidate map before installing it. A rejected call leaves
values and generation unchanged. These failures do not perform storage I/O.

### 3. Generation and batching

A namespace begins at generation zero. `set` and an accepted `apply` or
`apply_guarded` advance it once, regardless of batch size or equal values.
`remove` advances it only when the key exists. Removing an absent key returns
the current generation. Guards compare the expected generation under the same
mutex as the write. Counters are local to an owner and plugin, except that a
fork inherits the captured counter.

### 4. Where the store is exposed

`PluginRegistrar::state()` supplies the bound handle during registration.
`SessionReadyContext.state` supplies it at readiness. Hook closures retain the
registration handle; shared hook contexts need no plugin-id selector.

Materialization hydrates namespaces before registration so reads and size checks
see durable data. Initialization applies accepted registration edits to that
snapshot once, then `session_ready` observes the resulting state. The plugin
view of the host cannot export other namespaces.

### 5. Read-your-writes, and the durability boundary

Each call or batch is atomic. All cloned handles observe accepted writes.
The resident map can be ahead of the committed checkpoint; the next runtime
boundary commits the captured state. A turn failure does not roll back the map.
A cold rebuild observes the committed state and loses any uncommitted tail.
Plugins that require agreement with a committed outcome write from an
appropriate committed-path hook.

### 6. The checkpoint component

The `plugin_state` keyed component contains ordered plugin namespaces, each
with generation and ordered values. Content addressing gives the body its
`BlobRef`; the generation is part of that body, rather than a manifest column.
The runtime recaptures when namespaces exist and the component is absent or
its captured generations differ. Otherwise it retains the reference.

Per-key generations add no useful invalidation boundary because capture writes
the whole component. A resident hydration that would rewind accepted writes
or replace equal-generation values is refused.

### 7. Fork

Fork initialization uses a deep copy of captured parent namespaces and their
generations. It preserves non-resident namespaces as well as resident ones.
Parent and child writes are independent; there is no merge. Content-addressed
storage can deduplicate unchanged bodies. Retention policy governs how long
the session's checkpoint contents remain available.

## Alternatives considered

Snapshot callbacks and plugin-owned revision counters delegate freshness to
the mutating plugin. Mediation makes freshness a runtime fact. Returning mutable
values would allow writes without advancing it. Async setters imply an I/O or
commit boundary that this handle does not provide.

A global namespace has no session lifecycle owner. Append-only plugin logs add
another write algebra and retention contract; capped JSON arrays already serve
bounded list state. Per-key dependency tracking adds bookkeeping to a component
that is captured whole. Unconditional capture remains correct but needlessly
serializes unchanged state. File-store escape hatches move plugin durability
outside the runtime-owned boundary; larger state belongs in host storage.

## Consequences

The plugin owns its JSON schema and any value migration inside the namespace.
Lash owns acceptance generations, serialization, and checkpoint capture.
Derived caches can be rebuilt from the store. Accepted writes become durable
at a later runtime boundary, so acceptance and commit have distinct lifetimes.

## Code references

- `crates/lash-core-execution/src/plugin/state.rs:12-28,66-280,322-404` implements the handle, bounds, batches, and hydration.
- `crates/lash-core-execution/src/plugin/runtime_impl.rs:306-316` orders initialization and readiness.
- `crates/lash-core-execution/src/plugin/registrar.rs:511` delivers the registration handle.
- `crates/lash-core-store/src/plugin_state.rs:7-18` defines checkpoint namespaces.
- `crates/lash-core-store/src/session_state.rs:1083-1110` gates capture by generations.
