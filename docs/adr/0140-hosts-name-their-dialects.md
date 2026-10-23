# 0140: Hosts name their dialects

Status: Accepted. Builds on
[ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md) (dialects lower to
one kernel) and
[ADR 0123](0123-model-code-runs-in-resettable-worker-processes.md) (workers).

## Context

Hosts choose which source dialects their workers serve. A host that needs
one parser should be able to build and install that dialect alone, while
using the same linked execution model as a host that serves both.

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
worker library. `finish(&library)` refuses an embedding with no
dialect (`EmbedError::NoDialect`), and `dialect` refuses one named twice. The
shipped `lash-vm-worker` executable installs every dialect it was built with
(`built_dialects`); built with none, it refuses to start.

Hosts install code through the [package model](../host-packages.md).
A host package adds operations, functions and value types that installed
dialects expose in their own syntax. Installing a package does not add a parser.

### Dialects are build features, none a default

`dialect-typescript` and `dialect-python` are features of the facade
(`lash-runtime`), `lash-vm-worker`, `lash-vm-runtime` and
`lash-vm-releases`, which builds the immutable helper image. Each is off unless a
host names it. A dialect's feature
builds its helper library, and in a worker links its parser and native
implementations. The ECMAScript regular-expression and date extensions and the
WHATWG URL extension are TypeScript's: a Python-only build links none of
them, and a TypeScript-only build links no Python parser.

The facade's `dialect-typescript` does not install a parser in the host
process. The separate `typescript` feature selects that source front end and
printer (`lash::typescript`); `python` does the same for Python. A worker
embedding links the parsers selected by its dialect features.

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

### Linked code and worker native declarations have separate lifetimes

The build emits immutable helper functions, addressed by FunctionId, in one
WorkerLibrary image per process or worker. Helpers are outside package
Documents, publications and Links. A document names its direct build-library
references in LibraryDependencies; ordinary operation slots still supply every
service authority. A parked root records HelperLibraryId. Equal identities need
no compatibility walk; a changed image checks the saved live helper reach and
refuses a missing exact FunctionId before execution resumes.

The host publishes user- and host-installed code under scoped provenance and
retains its full Link DAG. Each run loads those reached Documents and Link slot
tables before execution. The round records its accepted Package Advertisements at round start;
recovery reads that recorded namespace, never a newly selected advertisement.

## Consequences

- A helper executable is built for its host's dialects. Recipes that build it
  with Cargo pass the dialect features.
- A worker must hold the run's recorded dialect and meet its machine requirements.
- A new round records accepted plugin advertisements with committed config. Existing cells, saved callables and workflows retain their defining Links. Opening or printing a saved workflow does not rebind it; publication of independently edited content creates a new definition fulfilled in the host-selected environment.
- Model tools require explicit provider registration; service Packages do not
  change model exposure.

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
