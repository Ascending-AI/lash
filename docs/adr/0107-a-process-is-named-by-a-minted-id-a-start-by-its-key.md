# 0107: A process is named by a minted id, a start by its key

## Status

Accepted.

## Context

A start needs an idempotency key, while every later operation needs the
identity of one process lifetime. Keeping those identities separate lets a
retry find its retained result without making a caller's name the address
of every future process registered under that name.

## Decision

### 1. A process id is minted, never chosen, never reused

`ProcessId` is `p_` followed by 32 lowercase hexadecimal digits of a
UUIDv7. The registrar mints it inside the registration transaction, after
admission checks. Parsing and deserialization validate that spelling and
the UUID version and variant. Production registration mints a fresh id;
fixture generators can install a sequential test mint.

Handles, process effect openers and process-record artifact referrers name
that id. While its tombstone is retained, a pruned id answers
`ProcessNoLongerRetained`; an unknown id answers `ProcessUnknown`. Tombstone
compaction removes that distinction. A display label is descriptive data;
there is no label-to-process lookup.

Evidence: `crates/lash-sansio/src/identity.rs:212`,
`crates/lash-core-store/src/process_identity.rs:19`, and
`crates/lash-sqlite-store/src/process_registry/registration.rs:74`, and
`crates/lash-core-execution/src/runtime/process/registry_concerns.rs:846`.

### 2. A start is keyed by an optional `StartKey`

A `StartKey` is a framed digest in the `lash.process-start-key` v1 family.
Its preimage identifies the admitted operation, not the submitted content,
source, compiler or minted result. The start paths have separate namespaces:

- A tool intent uses its recorded replay key.
- A trigger delivery uses its occurrence, subscription, subscription
  incarnation and revision.
- A host or remote caller supplies bytes to `StartKey::for_host`.
- A keyless host start uses its admitted scope and start ordinal, so replay
  issues the same key.

Host bytes alone determine a host key across the store set. Originators and
deployment namespaces that share a store share that key space. The host
owns any partitioning policy; Lash performs no authorization decision.

A tool-intent or trigger key is trusted. A repeat returns the retained
process as `Existing` regardless of the retry's submitted content. A host
key, supplied or keyless, fences the start. Its input, lifetime, ancestry,
session capability, identity, event types, provenance, wake target and
environment must match the retained registration. A mismatch returns
`PluginError::StartKeyConflict { start_key }`, whose error names only the key.

The input is compared as the host stated it. A session-turn start that names
no model records the default binding its core minted when it first
registered; that binding is derived, not stated, so the fence leaves it out,
and a core mints none while a start is retained under the key. A retry after
a catalog edit or a change of default is the same start and is returned the
retained process with the binding it recorded (FIG-4531).

The request's key is private. Host entry points accept only the host family
and refuse another family as `start_key_family_refused`. Derivation and
parsing of internal families require `StartKeyDerivation`, which the facade
and runtime root do not export. After pruning, a key can register a fresh
process with a fresh id.

Evidence: `crates/lash-core-store/src/process_identity.rs:190`,
`crates/lash-core-execution/src/runtime/process/validation.rs:1003`, and
`crates/lash-core-execution/src/runtime/process/model/start_request.rs:217`.

### 3. The start effect is addressed by the key

A journaled start is `process:start:{start key}`. Its staging referrer is
`Start(key)`. A missing key refuses as `process_start_key_missing`. The
recorded result contains the minted id and `Created` or `Existing`, so replay
answers the disposition the first execution observed.

Host and remote start receipts carry the id, key and disposition. A repeat
that returns `Existing` releases its staged content rather than adopting it
into the retained process.

Artifact cleanup must preserve a concurrent start's committed content. A
start that finds its staging referrer ended holds the content under its own
`ProcessRecord(id)`; cleanup protects committed rows it encounters under the
same referrer. [ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md)
§3.3 owns those artifact rules.

Evidence: `crates/lash-core-execution/src/runtime/effect/envelope.rs:910`,
`crates/lash-core-execution/src/runtime/process/start_staging.rs:327`, and
`crates/lash-restate/src/controller/process_command.rs:281`.

### 4. A declared start answers a slot

A declaring tool answers `{"__start_slot__": <intent index>}`. The attempt
coordinator replaces the slot with the realized process handle before
projecting a successful result to the model or cell. If the declared start
does not realize, the result is the typed `process_start_unrealized` failure.

Registration records a declared start's consumer hold atomically with its
process row. Retention cannot prune a held row. The consumer releases the
hold after incorporating settlement; its opener's close also releases the
hold. Cancellation and lifetime are separate obligations, as specified in
[ADR 0116](0116-tools-are-opaque.md) §3.

Evidence: `crates/lash-sansio/src/handle.rs:168`,
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:803`,
`crates/lash-sqlite-store/src/process_registry/registration.rs:104`, and
`crates/lash-sqlite-store/src/process_registry/prune_api.rs:119`.

### 5. A trigger delivery binds its process after the start

A delivery reservation has no process until the start registers. The router
then binds its minted id. Recovery starts an unbound reservation under its
delivery key and binds the result. A crash between registration and binding
therefore converges on the process retained under that key. Binding also
settles the delivery obligation, as specified in
[ADR 0109](0109-store-to-engine-delivery-is-an-outbox-of-obligations.md) §3.

A bound delivery never mints again, although §2 lets a key register afresh
once its process is pruned. A delivery's key finds nothing after the bound
process is pruned, while the binding outlives it. When the key finds nothing,
the registrar reads the delivery's binding in the same transaction. A bound
delivery registers nothing and refuses as `TriggerDeliveryBound`, and its
emitter answers the bound process (FIG-4369). The binding is written once, and
a bind precedes the prune of its process, so the check sees every bind whose
process is absent from the key lookup.

Evidence: `crates/lash-core-execution/src/triggers/router.rs:666`.

### 6. Durable identity shapes

Persisted and wire process references use the minted id. Start envelopes
carry the key separately, and the start's recorded receipt carries its
result. Process identity parsing is strict. Durable-format compatibility is
owned by [ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md);
the pre-1.0 version freeze applies to these shapes.

Evidence: `crates/lash-sansio/src/identity.rs:304` and
`crates/lash-core-store/src/process_identity.rs:303`.

## Consequences

Callers retain the returned `ProcessId` to address the process. A readable
label cannot serve as that address. Reusing a start key after pruning creates
a different lifetime, while a journal that retains its start result returns
its recorded id. Declared-start consumer holds protect the result until the
consumer settles it.

A caller-chosen process address would combine retry identity and lifetime
identity. A separate incarnation would require every handle and command to
carry another identity. A minted id names the lifetime directly. Host-key
content checks make retries explicit without turning submitted content into
the idempotency key.
