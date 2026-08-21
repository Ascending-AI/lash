# Checkpoint components generalize to a keyed set

## Status

Accepted. Amended 2026-08-10 (FIG-1195): the inline-versus-leaf line comes from
a named constant rather than a store blob profile. Amended 2026-08-11
(FIG-1257): that line applies to every value shape. Amended 2026-08-21
(FIG-1728): RLM snapshot v14 removes the scratch-file section.

## Context

The RLM execution-state snapshot currently re-serializes every live global,
scratch file, and deferred resolution whenever any of them changes. It encodes
that state as JSON and then embeds the JSON string in another JSON document.
Three research sessions consequently reached commit with 1.52 MiB, 1.32 MiB,
and 1.24 MiB checkpoints against the 1 MiB budget even though a turn had
changed only part of the retained state. Binary scratch files are also reduced
to empty strings, and collection failures can discard the file set without a
diagnostic.

[ADR 0048](0048-checkpoint-component-identity-is-a-backend-contract.md)
establishes a backend-independent content-addressed identity and hydration
contract for three fixed checkpoint components. A body mints a ref, a ref
without a body means unchanged, and an unknown ref is an error. The stores
already write deduplicated component blobs in the checkpoint commit
transaction and exclude unchanged bodies from the commit budget. Execution
state needs that mechanism at a finer granularity, rather than an attachment
store whose writes are outside the transaction and whose references do not
have the required session visibility or retention semantics.

The precedent survey supports one contract across backends. Git trees and
blobs provide the root-manifest and reusable-leaf shape. LangGraph's Postgres
saver deduplicates blobs by channel and version, while its SQLite saver writes
one whole checkpoint blob; that divergence is precisely what a store-level
contract must prevent. Restate addresses state cells by key rather than
rewriting a whole map.

## Decision

**Amendment (2026-08-21, FIG-1728).** RLM snapshot v14 removes the `files`
section and file-body leaves from the execution-state root. The remaining root
contains globals and deferred resolutions; older snapshots fail closed with the
standard drain-or-recreate remedy. References to files below record the
superseded v13 decision rather than a compatibility path.

The checkpoint-component contract is a keyed set rather than three fixed
slots. An execution-state root is a small typed component with sections for
globals, files, and deferred resolutions. The root contains every stable
logical key and either its inline value or a content-addressed leaf descriptor.
The logical key exists only in the root; a leaf ref contains the content hash,
not the logical key.

Each keyed component has three commit states: a present body, an unchanged ref
without a body, or deletion by absence from the new root's complete key set.
Stores mint refs for present bodies and hydrate unchanged refs as required by
ADR 0048. A ref that the store cannot resolve fails with the typed
missing-component error, extended to carry the dynamic component key. No
backend may collapse the keyed set back into one opaque execution-state blob.

Execution state uses hybrid granularity:

- every value body below the inline-versus-leaf size line is inline in the
  root; and
- every value body at or above the line is a leaf, regardless of whether it is
  a scalar, composite, byte value, or file.

The withdrawn structural rules were proxies for size assumptions that fail in
both directions. A scalar string can hold a scraped page large enough that
inlining many of them makes every commit track total session size, while a
scratch file can be so small that its root reference and manifest row cost
several times more than its body. Shape therefore does not decide checkpoint
granularity; measured encoded body size does. File bodies are measured as their
verbatim bytes, and global values as their canonical typed MessagePack bytes.

That line is structural and lives in exactly one place: the named constant
`lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES`, which every protocol
plugin's capture consumes. It is deliberately **not** taken from a store's
blob-compression profile, which this decision originally reached for. Those
profiles answer whether bytes should be compressed, not whether a value is
worth its own component; there are three of them and one never compresses at
all, so reading one would both make snapshot shape depend on the configured
backend and leave the granularity of a checkpoint tied to an unrelated knob.
The constant's value follows from what each choice costs per commit — an inline
value costs its encoded length because the root is re-encoded in full every
commit, while a leaf costs a root reference plus a manifest row and nothing
else. The production budget accounting measures a retained file leaf at 273
bytes of fixed overhead and the inline/leaf break-even at a 272-byte body. The
512-byte line stays comfortably above that marginal point without introducing
a new fixed threshold. Small values stay inline, while values at or above the
line become leaves under stable logical keys. Content-defined chunking is
deferred.
It may later operate inside one oversized leaf, as Git added packfile deltas
beneath its tree/blob model, without changing the checkpoint contract.

Root and value leaves use typed MessagePack. File leaves contain their verbatim
raw bytes. Encoding observes these normative rules:

- non-finite floats round-trip, and every NaN is normalized at encode time to
  one bit pattern;
- values that cannot be represented fail at encode time with a typed error
  naming the path to the offending value; they are never silently replaced
  with null and never first discovered during restore;
- dynamic maps are sorted, only typed structs are encoded, and
  `#[serde(flatten)]` is forbidden;
- byte fields use `serde_bytes` rather than MessagePack integer arrays; and
- the minimum encoder version is pinned, including the rmp-serde fix from PR
  257, and named-field encoding is pinned by conformance test.

An inline file body remains verbatim bytes in the typed root and uses
`serde_bytes`; it is never interpreted as UTF-8.

The canonicalization conformance test encodes the same logical state across
runs and dependency bumps and asserts byte equality. It is the authority for
the named struct representation despite rmp-serde accepting both map and
sequence struct encodings. Recursive value decoding is depth-bounded. Typed
structs only also structurally exclude the arbitrary-type reconstruction that
caused LangGraph's GHSA-fjqc-hq36-qh5p deserialize-code-execution class.

MessagePack does not preserve unknown fields. The complete serde evolution
toolkit is therefore append-only optional fields expressed as `Option` with
`#[serde(default)]`; there is no tolerant-read or unknown-field-preservation
contract. Each persisted component descriptor carries its own encoding
version. A version mismatch is a typed error that identifies the component and
names drain or recreation as the remedy, allowing any later cutover to be
scoped per component.

Component hashes remain SHA-256 over the uncompressed logical bytes;
compression happens only after hashing. The commit budget counts the root body
plus the bodies of changed leaves. Unchanged leaf refs are free. Leaves and the
root commit in the same transaction. Git's loose-object design admits a race
in which garbage collection can remove an object after it is written but
before a ref makes it reachable; the single checkpoint transaction admits no
equivalent window.

Reclamation remains host-owned operational policy under
[ADR 0014](0014-operational-policy-stays-with-the-host.md) and
[ADR 0023](0023-retention-stays-a-parameterized-host-lever.md). The root
provides the leaf reachability manifest, but Lash does not infer a collection
horizon or introduce an internal garbage-collection policy.

Encoding follows the surveyed fail-early practice: Temporal rejects when no
converter exists, Metaflow names an unpicklable artifact, and Ray identifies
the nested attribute path. It rejects LangGraph's restore-time decode-to-null
behavior. Migration follows platform practice instead of embedded-checkpointer
practice: Temporal, Restate, and DBOS pin deployments and drain incompatible
state, while LangGraph and Metaflow retain read-side compatibility decoders.
Lash takes the platform side.

The cutover raises the RLM snapshot version and changes the store schema. Hosts
must drain or recreate retained execution state before deploying it, following
the discipline in
[ADR 0055](0055-lashlang-execution-bounds-span-durable-process-lifetimes.md).
The record-schema-version reject pins old records to the old deployment and
prevents them from being interpreted under the new schema. There are no
compatibility decoders; the record and per-component version checks make that
policy enforceable.

The FIG-1257 amendment changes durable root shape decisions and therefore
raises the RLM snapshot version from 7 to 8 and the checkpoint-component
encoding version from 1 to 2. It adds no compatibility decoder and no store
schema change; retained version-1 components are rejected and must be drained
or recreated.

> **Historical versions.** The version numbers in this ADR record the state at ratification. The current values live in `lash::formats`; see `scripts/check_format_versions.py`.

## Consequences

- A commit grows with its changed execution state rather than every value the
  session retains, and all backends provide the same keyed deduplication
  behavior.
- Binary file contents remain exact, non-finite floats remain values, and
  unsupported state fails before commit with a path-specific typed error.
- Canonical bytes make content identity stable across runs and supported
  dependency bumps; canonicalization changes require an explicit component
  version cutover.
- The snapshot-version bump and store-schema change are breaking changes.
  Retained sessions must be drained or recreated; old records are rejected
  rather than decoded through a compatibility path.
- Content-defined chunking inside a large leaf remains available as a later
  storage optimization and is not part of this decision.
- Keyed leaves make Restate-style lazy hydration possible: a resumed execution
  could load a value only when it is accessed, reducing restore I/O and working
  state. The payoff is preserved, but lazy hydration itself is deferred.
