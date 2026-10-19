# 0140: Hosts name their dialects

Status: Accepted. Builds on
[ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md) (dialects lower to
one kernel) and
[ADR 0123](0123-model-code-runs-in-resettable-worker-processes.md) (workers).

## Context

Hosts choose which source dialects their workers serve. A host that needs
one parser should be able to build and install that dialect alone, while
using the same package installation and execution model as a host that
serves both.

Ruling (Sam, 2026-10-11): "There should be no default. It should be up to the
host to choose what dialect it wants, explicitly, and our Rust build should
allow specifying what to build with." A value most hosts customize has no
default.

## Decision

### No embedding without a choice

`lash-vm-worker` has no `standard` embedding. A host builds its embedding by
naming each dialect:

```rust
worker_entry(&|tuning| {
    Embedder::new()
        .dialect(typescript(tuning)?)?
        .dialect(python())
})?
```

`worker_entry` takes that constructor and binds it to the parent's
packages. `finish(&held)` refuses an embedding with no
dialect (`EmbedError::NoDialect`), and `dialect` refuses one named twice. The
shipped `lash-vm-worker` executable installs every dialect it was built with
(`built_dialects`); built with none, it refuses to start.

Hosts install code through the [package model](../host-packages.md).
A host package adds operations, functions and value types that installed
dialects expose in their own syntax. Installing a package does not add a parser.

### Dialects are build features, none a default

`dialect-typescript` and `dialect-python` are features of the facade
(`lash-runtime`), `lash-vm-worker`, `lash-vm-runtime` and
`lash-vm-releases`, which builds the package artifacts. Each is off unless a
host names it. A dialect's feature
builds its code package, and in a worker links its parser and native
implementations. The ECMAScript regular-expression and date extensions and the
WHATWG URL extension are TypeScript's: a Python-only build links none of
them, and a TypeScript-only build links no Python parser.

The facade's `dialect-typescript` also includes `typescript`, the front end
and printer in the host's own process (`lash::typescript`,
`WorkflowDocument::typescript`).

### A run names its recorded dialect, and a worker checks it

A session already records its dialect in its execution state and each cell's
envelope. A kernel process now records it too, in its creation settings
(`KernelRecordedSettings.dialect`), from the dialect the host named to
`KernelProcessPluginFactory::new`. Every run start carries the recorded
dialect (`Start.dialect`), read back from the record and never from live
configuration.

A worker whose embedding lacks that dialect refuses the run before it reads
the document or the parked state:
`RunRefusal::DialectNotInstalled { dialect }`, a terminal typed refusal.

### One package installation supplies parent and workers

The parent installs lash's code-only packages and host packages together.
Every worker derives its registry from that installation; the embedding
supplies natives by function identity. A shipped worker receives the
registry in a pool-start frame. The selected dependency-closed package set
chooses the names in scope for a cell. A parked run keeps an execution
closure containing its exact function identities.

## Consequences

- A helper executable is built for its host's dialects. Recipes that build it
  with Cargo pass the dialect features.
- A worker must hold the run's recorded dialect and exact code identities.
  Code-only packages and user-visible package surfaces share installation.
- New cells lower against the selected set. Saved-function relinking is
  in progress; changing the set does not relink a parked run or a published
  workflow.

## Executable evidence

- `crates/lash-vm-worker/src/embedding.rs`:
  `an_embedding_with_no_dialect_is_refused`.
- `crates/lash-vm-worker/tests/fresh_embedding.rs`:
  `a_host_package_installs_once_and_runs_in_a_single_binary_worker`.
- `crates/lash-vm-worker/tests/pool_laws.rs`: every pool law under a
  TypeScript-only and a TypeScript-and-Python embedding;
  `a_worker_refuses_a_run_in_a_dialect_its_host_did_not_name`;
  `a_python_cell_lowered_in_the_worker_runs_under_its_confinement`.
- The [host-package guide](../host-packages.md) describes package validation,
  pinning and worker distribution.
- `scripts/feature-coverage.toml`, lane `dialects`.
