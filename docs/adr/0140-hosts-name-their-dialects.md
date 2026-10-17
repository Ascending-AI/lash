# 0140: Hosts name their dialects

Status: Accepted. Builds on
[ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md) (dialects lower to
one kernel) and
[ADR 0123](0123-model-code-runs-in-resettable-worker-processes.md) (workers).

## Context

The worker had one embedding it gave every host, `standard`: the kernel
library, lash's extensions and TypeScript. Python existed only as an
embedding tests assembled by hand, and its helpers belonged to no helper
release, so a parked Python run pinned TypeScript's. A host could not choose
its dialects, and a host that wanted one dialect carried the other's parser.

Ruling (Sam, 2026-10-11): "There should be no default. It should be up to the
host to choose what dialect it wants, explicitly, and our Rust build should
allow specifying what to build with." A value most hosts customize has no
default.

## Decision

### No embedding without a choice

`lash-vm-worker` has no `standard` embedding. A host builds its embedding by
naming each dialect:

```rust
Embedder::kernel()?
    .dialect(typescript(tuning))?
    .dialect(python())?
    .finish()
```

`worker_entry` takes that constructor. `finish` refuses an embedding with no
dialect (`EmbedError::NoDialect`), and `dialect` refuses one named twice. The
shipped `lash-vm-worker` executable installs every dialect it was built with
(`built_dialects`); built with none, it refuses to start.

A dialect is one lash ships. An embedder cannot install a package of its own:
a run pins a helper release, and only a shipped dialect has one.

### Dialects are build features, none a default

`dialect-typescript` and `dialect-python` are features of the facade
(`lash-runtime`), `lash-vm-worker`, `lash-vm-runtime`, `lash-vm-library` and
`lash-vm-releases`. Each is off unless a host names it. A dialect's feature
builds its helper releases, and in a worker links its parser and the library
it ships with. The ECMAScript regular-expression and date extensions and the
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

### One helper release line per dialect

Each built dialect has one current helper release defined by the build from its sources. A new run pins that dialect's exact manifest; a build holding a different manifest refuses resume with a typed release mismatch. Parents and workers read the same generated definitions.

## Consequences

- TypeScript's release holds what the single release held, so a TypeScript
  run's pinned manifest is unchanged.
- A Python run pins Python's release, which a TypeScript-only build does not
  hold: it refuses the run by dialect before it would refuse the release.
- A helper executable is built for its host's dialects. Recipes that build it
  with Cargo pass the dialect features.
- A stored kernel process records its dialect; the 1.0 process image is
  re-recorded.

## Executable evidence

- `crates/lash-vm-worker/src/embedding.rs`:
  `an_embedding_with_no_dialect_is_refused`,
  `an_embedding_holds_only_the_dialects_its_host_named`.
- `crates/lash-vm-worker/tests/pool_laws.rs`: every pool law under a
  TypeScript-only and a TypeScript-and-Python embedding;
  `a_worker_refuses_a_run_in_a_dialect_its_host_did_not_name`;
  `a_python_cell_lowered_in_the_worker_runs_under_its_confinement`.
- `crates/lash-vm-releases/src/tests.rs`:
  `the_build_holds_the_releases_it_declares_for_each_dialect`,
  `a_dialects_release_holds_none_of_another_dialects_helpers`.
- `scripts/feature-coverage.toml`, lane `dialects`.
