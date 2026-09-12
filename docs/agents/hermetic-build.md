# Hermetic Rust builds

Lash uses Bazel 9.1.0 and rules_rs 0.0.110 for checkout-independent local
compilation. Cargo manifests and `Cargo.lock` remain the source of truth. The
generated BUILD files expose one Bazel action for each first-party library,
binary, example, benchmark, unit-test crate, integration-test crate, doctest,
and custom build script reachable in Cargo's resolved default workspace graph.
The complete Cargo metadata inventory also records targets whose required
features are outside that graph; their existing named Cargo feature recipes
remain authoritative. Third-party crates are imported from the same lockfile.
These labels are repository build surfaces; they do not expand the supported
SDK package surface described by ADR 0079.

Run every command from a checkout root. In an Orb, source `env.sh` before direct
Cargo commands; the entry script does this automatically.

`analyze` validates generated files and performs Bazel loading and analysis with
`--nobuild`; it does not run rustc and is not a substitute for `cargo check`.
`build` compiles and links the requested Bazel labels. `test` first builds and
then executes the explicit test labels. For an edit loop, request the narrowest
library, binary, or test label affected by the change.

```sh
# Analyze the generated graph and reject metadata or module-lock drift.
scripts/hermetic-build.sh analyze

# Compile the complete Cargo --workspace --all-targets shape.
scripts/hermetic-build.sh build

# Compile or run focused targets.
scripts/hermetic-build.sh build //crates/lash-core:lash-core
scripts/hermetic-build.sh test \
  //crates/lash-sansio:lash-sansio__unit_test \
  //crates/lash-sqlite-store:integration__test
```

These commands select the host shared executor by default. `--shared` makes
that choice explicit. It uses REAPI instance `orb` at
`grpc://127.0.0.1:45191`, requires the declared NativeLink runtime identity,
and fails when the executor is unavailable. NativeLink executes trusted builds
directly on the host. Hermeticity here describes pinned tools and declared
inputs, not an OS security boundary. `--local` executes actions in the checkout
for CI, bootstrap, and reproducing an executor-specific failure:

```sh
scripts/hermetic-build.sh --local build //crates/lash-core:lash-core
```

After changing workspace membership, a manifest, or `Cargo.lock`, regenerate
and review the graph:

```sh
scripts/hermetic-build.sh sync
git diff -- MODULE.bazel.lock tools/bazel '**/BUILD.bazel'
scripts/hermetic-build.sh analyze
```

`sync` is the only workflow that updates `MODULE.bazel.lock`. Normal commands
use lockfile error mode. The lock records module, crate-source, and toolchain
archive integrity; `rust-toolchain.toml`, rules_rs, and CI all select Rust
1.98.1. Published manifests keep `rust-version = "1.90"` as their compatibility
floor.

The shared caches live under `/home/sam/.cache/lash-bazel`; Bazel action keys use
declared repository-relative inputs, so two Orb paths can reuse the same
results. The executor has eight action slots. Each action declares one CPU and
2 GiB by default, Bazel queues at most eight jobs, repository loading uses four
threads, and each checkout's Bazel server has a 4 GiB heap ceiling. The Bazel
server remains in the caller's cgroup; remote compilation runs inside the
executor's `orb-heavy.slice` budget. Keeping a Bazel server alive preserves its
analysis cache.

For a golden refresh, prewarm the shared action cache once with:

```sh
scripts/hermetic-build.sh --shared build
```

New agents use the same command or request focused labels. A golden refresh
skips the full Cargo warmup. Cargo target directories remain lazy and scoped to
the gates below that need Cargo semantics; those gates use direct rustc rather
than a separate shared compiler-cache daemon.

Keep each retained Cargo gate's target directory unique to its checkout and
under `/home/sam/.cache/lash-cargo/`, which has room for compiler artifacts.
Do not place new cold targets on `/workspace`. These Cargo targets are separate
from Bazel's shared repository and action caches and can be removed with their
owning checkout after its Cargo-specific gates finish.

Before removing an Orb fork, stop its Bazel server and remove its workspace
output base:

```sh
scripts/hermetic-build.sh clean
```

Bazel maps each checkout to a hashed directory below
`/home/sam/.cache/lash-bazel/user-root`; `clean --expunge` removes only the
calling checkout's output base. It preserves the shared
`/home/sam/.cache/lash-bazel/repository` and
`/home/sam/.cache/lash-bazel/disk` caches. Orb removal hooks should invoke this
entry point before deleting the checkout.

The generated graph follows Cargo's resolved default workspace feature graph.
Cargo-owned test labels carry `manual` plus a reason tag, which keeps a broad
`bazel test //...` from treating an unconfigured service or path fixture as
proof. Use the existing Cargo recipes for these correctness contracts:

- `scripts/check_feature_coverage.py` and the explicit no-default-feature
  commands own feature-combination coverage. Cargo-required targets omitted
  from the resolved default graph are recorded with `cargo-feature-gate` in
  `tools/bazel/target-inventory.json`.
- Cargo doctests remain authoritative for rustdoc behavior. Bazel doctest
  labels provide inventory and focused iteration.
- The `lash-runtime` `ui` target owns trybuild compile-fail fixtures and their
  nested Cargo target cache.
- nextest profiles own workspace filtering, retries, and scheduling; the
  fault-matrix and simulation fixtures invoke nested Cargo builds.
- PostgreSQL, MinIO/S3, Restate, browser, Test262, judged-runbook, and other
  named live recipes own their services, environment, ignored-test selection,
  and runtime assets.
- fuzzing keeps its nightly `cargo fuzz` toolchain and corpus workflow.
- `cargo package`, the layered publisher, exact-SHA release validation, the
  release profile (`thin` LTO and stripping), and the `judged` profile remain
  Cargo-owned publication and artifact contracts.

The Bazel default is the development compilation graph. Timing comparisons
must use Rust 1.98.1, the resolved default workspace features, equivalent
optimization and debug flags, the same target set, and separately reported
cold repository, cold action-cache, warm, representative-edit, and second-Orb
runs. Compare focused Bazel edit builds with Cargo's existing incremental
`cargo check` path as separate operations: Bazel `build` produces linkable
artifacts, while Cargo `check` normally stops at metadata. Cargo release or
judged timings are not comparable to this graph.
