# Plugin state is a lash-mediated per-plugin store

## Context

Durable plugin state needs observable writes and a runtime-owned checkpoint
boundary. A plugin's private freshness assertion cannot prove that the state
captured at a boundary includes every accepted mutation.

## Decision

Lash owns a JSON key-value namespace for each runtime owner and plugin id.
For a session plugin the owner is its session. `PluginStateStore` binds that
identity once; reads and writes do not accept another plugin id. A namespace carries the plugin's declared nonzero `format_version`. State and
recorded config share that stamp and the factory's pure format codecs.

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
mutex as the write. Counters are local to a resident owner and plugin. Hydration
restores the checkpoint generation and retains a resident acceptance-token
high-water mark.
Changing the restored namespace advances that token, so `apply_guarded` never
accepts a token observed for different content. The next accepted write carries
the advanced token into the checkpoint. A cold rebuild or fork inherits the
captured counter without the old resident's uncommitted tokens.

### 4. Where the store is exposed

`PluginRegistrar::state()` supplies the bound handle during registration.
`SessionReadyContext.state` supplies it at readiness. Hook closures retain the
registration handle; shared hook contexts need no plugin-id selector.

The engine records pure initialization and conversion as one complete
`PluginTransitionRecord` before constructing capabilities. The request names
its effect address, runtime owner, retained session head or captured process
segment, and target plugin admission. A refused namespace or config keeps the
whole candidate unpublished. Inactive namespaces retain their stamp and values.

For a session, one fenced `RuntimeCommit` publishes the namespace checkpoint,
recorded admission and native view with the existing operation receipt. Replay
serves the recorded candidate, and acknowledgement loss reuses that receipt.
Factory build, registration and readiness reconstruct capabilities over the
published native view. These callbacks can read their bound namespace; a write
returns `PluginStateError::WriteScopeRequired`. There is no resident
materialization edit log. The plugin view of the host cannot export other
namespaces.

Retained handles accept writes only in an engine-owned recorded callback scope
for their own registry. A spawned background task inherits no such scope and
receives the same typed refusal. Accepted batches are recorded beside the
callback result, including terminal failures. Replay installs the complete
postimages before returning that result, without invoking the callback.

### 5. Read-your-writes, and the durability boundary

Each accepted call or batch is atomic. All cloned handles observe its writes.
The callback's journal records accepted mutations with its result; the next
runtime boundary publishes the captured state. An abandoned callback attempt
rolls back its unrecorded tail and invalidates its guard tokens. Cold replay
restores a completed callback's recorded edits even if the deployment died
before that next runtime commit.

### 6. The checkpoint component

The `plugin_state` keyed component contains ordered plugin namespaces, each
with format version, generation and ordered values. Content addressing gives the body its
`BlobRef`; the generation is part of that body, rather than a manifest column.
The runtime recaptures when namespaces exist and the component is absent or
its captured generations differ. Otherwise it retains the reference.

The `plugin_admission` opaque checkpoint component carries the transition
request and current native namespace/config view. It uses the existing
checkpoint blob and operation receipt machinery, with no second journal or
transition table. Resident adoption reads this native view and performs no
conversion. The separately encoded `plugin_state` component retains the
admission's writer formats.

Per-key generations add no useful invalidation boundary because capture writes
the whole component. Resident hydration adopts the recorded native namespaces
and drops an uncommitted tail. Acceptance tokens retain their high-water marks
without changing the restored checkpoint bytes.

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

The plugin owns its JSON schema and its pure migration and writer encoders.
Encoding takes the caller's explicit writer version. The host never selects it
from a live fleet read. Config stamps travel with options, recorded config and
process environments; state stamps travel with every captured `SessionPluginInit`.
Lash owns acceptance generations, serialization, and checkpoint capture.
Derived caches can be rebuilt from the store. Accepted writes become durable
at a later runtime boundary, so acceptance and commit have distinct lifetimes.

## Code references

- `crates/lash-core-execution/src/plugin/state.rs` implements scoped writes, bounds, batches and native hydration.
- `crates/lash-core-execution/src/plugin/state/effect.rs` records and restores callback edits.
- `crates/lash-core-execution/src/plugin/transition.rs` defines complete transitions and checkpoint native views.
- `crates/lash-core/src/runtime/shift/plugin_transition.rs` prepares and publishes the session transition.
- `crates/lash-core/src/runtime/process_runtime.rs` adopts a process segment's recorded transition.
- `crates/lash-core-execution/src/plugin/runtime_impl.rs` reconstructs read-only capabilities.
- `crates/lash-core-store/src/session_state.rs` captures the checkpoint components.
