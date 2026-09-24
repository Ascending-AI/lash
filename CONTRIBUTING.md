# Contributing to Lash

Feature requests and bug reports are welcome — open an
[issue](https://github.com/Ascending-AI/lash/issues).

At this alpha stage, detailed write-ups help more than drive-by PRs. The
internals are still moving fast, so open an issue before starting a substantial
implementation and agree on the shape first.

To understand how the runtime fits together, start with `CONTEXT.md` for the
domain language and `docs/adr/` for the decisions behind the crate layout,
turn/effect boundary, and plugin model.

Use `mod` declarations and explicit re-exports for hand-written Rust source.
Reserve `include!` for build-script-generated code and assets; keep text assets
in `include_str!` and binary assets in `include_bytes!`.

## Development workflow

Lash uses trunk-based development. `main` is the only long-lived branch and is
kept releasable.

1. Update `main` and create a short-lived branch.
2. Make one focused change and run the relevant local checks.
3. Open a pull request into `main`.
4. Keep the branch current and merge only after required CI is green.
5. Delete the branch after merge.

### Local build and test loop

On the shared development box, use one Kiln fork per concurrent change. Kiln
creates a warm copy-on-write checkout and prints a path ending in `/merged`:

```sh
fork="$(kiln fork lash my-change)"
cd "$fork"
. ./env.sh
kiln test //crates/lash-core:lash-core__unit_test --test_arg=<name>   # while editing
python3 scripts/dev-test.py   # before calling it done; never starts Postgres/S3/E2E
```

Source the fork's `env.sh` before **any** Cargo command. It selects the fork's
private target directory and applies the shared machine's build and test
budgets. `kiln test` with no labels runs `//:dev_tests` — the developer
suite, which is the `//:workspace_tests` PR partition minus the dev-deferred
slow binaries — and `kiln test //:workspace_tests` runs that PR
partition exactly as CI does. When
the change has merged, remove the fork with `kiln rm lash <name>`. Never write
under a `golden-*` directory, remove a fork with `rm -rf`, or set `CARGO_*`
variables by hand.

On machines without Kiln, plain Cargo is the portable fallback. Preserve the
workspace feature graph and lockfile with `--workspace --all-targets --locked`;
use `--no-fail-fast` when claiming a full `cargo test` run. Use the repository's
named Cargo, `just`, or script recipes for contracts that need more than the
portable default-feature run.

| Command | Coverage |
| --- | --- |
| `kiln test` | `//:dev_tests`: the deterministic developer suite; `//:workspace_tests` adds the dev-deferred binaries for the PR partition. |
| `scripts/ci/with-service.sh <pg14\|pg16\|pg18\|s3\|all> -- bash scripts/ci/store-tests.sh <suite>` | One PostgreSQL or S3 (Garage) suite, against a container this command starts and removes. |
| `python3 scripts/dev-test.py` | `//:dev_tests` narrowed to the changed package directories (`:all` each); a shared input widens to the whole suite. Refuses live store URLs. |
| Named Cargo recipes | Tests and checks that require Cargo-owned semantics or assets. |

`scripts/ci/with-service.sh` is the same wrapper the `Test Postgres store` and
`Test S3 store` jobs run each suite inside, so a local run and CI take one code
path: it starts the CI image on a free ephemeral port, waits for readiness,
exports the connection settings, and removes the container on success, failure
and Ctrl-C alike. Run it with no arguments to list the services, and end a
local run reading the service-shaped cases it did **not** cover, each with the
exact recipe. See
[`docs/agents/hermetic-build.md`](docs/agents/hermetic-build.md).

Cargo remains for tests that invoke nested Cargo, trybuild fixtures, service
gates without a Bazel route, nightly fuzzing, publishing, and judged
or release profiles. Use `kiln build //:feature_lanes`, `kiln test
//:feature_lane_tests`, and `kiln clippy //:feature_lane_clippy` for feature
coverage. Use `kiln clippy` for workspace linting and `kiln fmt -- --check`
for formatting.

Install the repository's commit hook in each regular checkout with
`prek install --hook-type pre-commit`; new warm forks install it automatically.
The hook formats Rust source, including the enrolled `include!` files. When it
changes a file, the commit stops so you can review the result, stage the files
you intend to commit, and retry. The hook never runs `git add`, so it cannot
silently include unrelated or partially staged changes.

Keep local validation proportional to the change:

- Run cheap formatting and static checks relevant to the files you changed.
- For behavior changes, run the narrowest regression that proves the changed
  behavior. `python3 scripts/dev-test.py` runs the developer suite narrowed to the
  changed package directories. `scripts/fast-test.sh` is an optional broader
  iteration aid when reverse-dependency coverage is useful; high-fan-out
  crates can still select a large part of the workspace. Neither starts
  Postgres, S3, or E2E.
- Add a targeted live recipe only for a named durability or behavior risk that
  the current CI plan does not exercise. Merely touching `lash-core` or
  `lash-restate` does not require running both durable geometries locally. Implementer loops never run those live gates.

For Rust compilation, target analysis, and focused unit or integration tests,
use the checkout-independent Bazel workflow in
[`docs/agents/hermetic-build.md`](docs/agents/hermetic-build.md). Its default
entry point uses the shared local executor and cache; the named Cargo recipes
retain feature-matrix, service, trybuild, fuzz, judged, packaging, and
release semantics.

`just push-gate` and the `just confidence*` lanes remain available as explicit
full diagnostics before an unusual-risk change, release work, or when a user
requests them. They are not routine push or merge prerequisites. Stop once the
focused evidence is green; CI and independent review supply the broad merge
proof rather than repeating the same broad suite locally.

### Required checks and the merge queue

`ci.yml` subscribes to `merge_group`, so a queued pull request is validated from
its own `gh-readonly-queue/main/pr-<n>-<sha>` ref. Its plan job classifies the
exact diff and selects the correctness families configured for that event.
Pull requests and merge groups run the minimal core board: the Bazel partition
(tests and the workspace compile), lint, and the hygiene jobs, plus
path-gated seal, store and repository-gate jobs. The single `CI conclusion`
job rejects failed, cancelled, missing, or incorrectly skipped correctness
jobs and is the aggregate merge context.

`Feature lanes` is the one job that proves every non-default feature
resolution. Each command of `scripts/feature-coverage.toml` is compiled on the
Kiln pool as Bazel targets carrying that command's exact feature resolution,
generated by `tools/bazel/generate_build_files.py`, so the gate costs the
pool's critical path rather than a sequential `cargo check` per lane on a
4-vCPU runner. It is dispatch-only: pull requests and merge groups run the
minimal board, and the release dispatch is its sole home alongside deferred
Unicode and the lashlang consumer.
`docs/agents/hermetic-build.md` records how the resolution is computed and
reconciled against Cargo, and the one faithfulness limitation that remains.

Nothing heavy runs automatically on a push to `main`. `ci.yml` has no `push`
trigger at all: the queue already validated the exact tree that main
fast-forwards to, so a second automatic run over the same tree bought nothing.
The heavy families — `Test heavy suites`, `Test S3 store against Garage`, both
`Functional E2E` jobs, `Restate + Postgres + S3 Workers` and its coverage
summary, `Fuzz smoke`, `Stack budget`, `Test deferred Unicode suites`,
`Feature lanes`, `Lashlang Git consumer` and `Build worker release artifacts` —
run on a manual `workflow_dispatch` of `ci.yml`, which is exactly the full
profile release.yml certifies against. The
`Release cache` and `Seal cache` warmers are manual for the same reason:
dispatch `Release cache` to warm the `linux-release` cache that release.yml and
perf.yml restore, and `Seal cache` after a change to `Cargo.lock`, a manifest,
`rust-toolchain*`, `.cargo/**` or the toolchain actions. Both are non-gating
warmers: a cold cache costs wall clock, never correctness.

The workers E2E family runs on full-profile (`workflow_dispatch`) runs and on
pull requests carrying the `ci:workers` label. It does not execute in the merge
queue. Run a local worker recipe only
when a changed behavior needs earlier evidence or falls outside that CI
coverage; name that risk and recipe in the PR.

Pull requests land through the `main` merge queue, which revalidates every entry
against the true merged base before it lands — never by direct merge. Keep the
ruleset's required context aligned with the aggregate `CI conclusion`; renaming
that job without updating the ruleset will wedge every queue entry behind a
check that can never report.

## Concurrent local gates

`just push-gate`, the `just confidence*` batteries, and their container-backed
E2E recipes are isolated by checkout root. On the shared box that root is a
Kiln fork's `/merged` checkout; the scripts retain `WORKTREE` in internal
variable and command names for compatibility with ordinary Git worktrees. They
derive a stable slug by lowercasing the basename of the physical checkout root,
replacing non-alphanumeric runs with `-`, trimming leading or trailing `-`, and
appending the first eight hex digits of a stable checksum of the absolute
checkout path. Thus two checkouts with the same basename still have distinct
identities. Container names, fixed Compose projects, persistent external
network names, default evidence paths, and default host ports all include or
derive from that slug.

Each absolute checkout path hashes with `cksum` into one of 90 disjoint 50-port
blocks spanning 61000–65499, above Linux's default ephemeral range. The lane
offsets are stable:

- `+0..+9` attachment/usage workbench PostgreSQL, selected by the workbench
  port's last decimal digit;
- `+10` push/confidence PostgreSQL, `+11` push S3, `+12` mutation PostgreSQL;
- `+20..+23` agent-service Restate and endpoint;
- `+30..+34` agent-workbench Restate, endpoint, and PostgreSQL;
- `+35..+37` slack-clone full-host platform, bot, and HTTP MCP server;
- `+35..+39` effect-group-conformance Restate (admin, ingress, node, two
  endpoints); it shares `+35..+37` with slack-clone and is serialized by the
  checkout lock;
- `+40` distributed-worker S3;
- `+41`, `+43..+46` process-operations S3, Restate, and PostgreSQL;
- `+47` version-bump recreation PostgreSQL.
- `+48` slack-clone live-model platform.

Explicit existing environment overrides such as `LASH_PUSH_GATE_PORT_BASE`,
`LASH_PUSH_GATE_POSTGRES_PORT`, `LASH_CONFIDENCE_OUT_DIR`, and each recipe's
named port/container/artifact variables remain authoritative escape hatches.
If two concurrently active checkouts select the same block, set
`LASH_GATE_SLOT_OVERRIDE` to an unused integer from `0` through `89` for one
gate; this changes its derived port base while preserving its path-qualified
ownership identity. The refusal prints this override and the occupied lock
path.
The default confidence evidence root is
`target/confidence/<worktree-slug>/` for local runs. CI explicitly pins
`LASH_CONFIDENCE_OUT_DIR` to `target/confidence` so its established artifact
upload and summary paths are unchanged.

Every checkout uses a fixed external network named `lash-e2e-<worktree-slug>`.
Scripts create it idempotently and never delete it, because host network
watchers treat Docker network add/remove as interface churn. Compose projects
are fixed per checkout rather than per run. Their repeated `postgres`, `s3`,
and `restate` aliases are safe only because the worktree lock and labeled
leftover check prevent two lane projects from sharing this network at once. A
nonblocking worktree lock rejects a second same-checkout battery with exit 73.
The refusal names the owner PID, lock path, and exact orphan remedy. Compose
leftovers produce a project-qualified `docker compose ... down -v
--remove-orphans` remedy; direct containers use `docker rm -fv`. Lock state is
pinned to `/tmp/lash-gate-<uid>` so interactive, cron, and systemd-run gates
coordinate through the same identity regardless of `XDG_RUNTIME_DIR` or
`TMPDIR`; lock descriptors are not inherited by gate children. Lock acquisition
allows a bounded two-second handover for a holder whose owner just exited,
while a slot lock turns a residual hash collision into a clean refusal rather
than a host-port race.

After upgrading from the older global-name gate layout, remove pre-change
unlabeled state once, after confirming no old gate is running. In particular,
remove the old distributed-worker project with the current Compose file's
required values supplied only for configuration parsing:

```sh
source scripts/ci/s3-service.sh  # the S3 service values the Compose file reads
LASH_GATE_WORKTREE_SLUG=legacy LASH_E2E_S3_PORT=1 \
LASH_E2E_BIN_DIR=/tmp LASH_E2E_NETWORK=lash-e2e \
  docker compose -p restate-postgres-workers \
  -f runbooks/restate-postgres-workers/docker-compose.yml \
  down -v --remove-orphans
```

Remove obsolete `lash-*-push-gate-*` containers explicitly, and remove the old
`lash-e2e` network only when no container is attached. New gates never remove
unlabeled legacy state for you.

To prove the live contract against another checkout containing the same
change, run:

```sh
just gate-worktree-concurrency-check /path/to/peer-kiln-fork/merged
```

For two Kiln forks, pass the peer's `/merged` path. The check runs PostgreSQL,
S3, and Restate smokes concurrently in both checkouts, then proves a second
same-checkout run refuses cleanly. Evidence is written below
`target/gate-concurrency-proof/<worktree-slug>/` unless
`LASH_GATE_PROOF_OUT_DIR` overrides it.

### Machine load, as distinct from gate isolation

Checkout isolation makes concurrent gates *correct*; it does nothing about the
machine they share. Every concurrent `just push-gate` compiles the whole
workspace, and an unbudgeted build sizes itself from `nproc`, so several gates
at once oversubscribe the box. Two limits apply on a Kiln box, and both are
absent on CI runners, which get a runner per job and have nothing to share:

- **How wide one gate goes.** The build width comes from the environment —
  `CARGO_BUILD_JOBS=8` and `NEXTEST_TEST_THREADS=4`, exported by the Kiln fork's
  `env.sh` — not from `nproc`.
- **How much of the box Cargo can take.** The Kiln `cargo` wrapper runs every
  compile-shaped command in its own transient cgroup scope (800% CPU, 16 GiB)
  inside the fixed `kiln-heavy.slice` aggregate, which the NativeLink executor
  shares as its majority tenant. Gates are therefore capped rather than queued:
  nothing waits for a slot, and no lane can starve the executor or the box.
  `kiln cgroup status` reports the live picture. The box-wide `heavy-slot`
  semaphore that `push-gate.sh` used until 2026-09-13 is retired.

Raise a budget only for a specific measured need. An unbudgeted gate does not
finish sooner; it makes every other one finish later.

There is no `staging` branch. Preview work belongs in pull requests, while the
merged product state lives on `main`.

## Rust toolchain

The toolchain is pinned, not floating: a new Rust release changes nothing
until a pull request bumps it, so new-lint fallout lands inside a reviewed
diff instead of turning `main` red overnight (FIG-1672). The pinned version
is stated in three places that must move together — `rust-toolchain.toml`
(local Cargo/rustup), the `toolchain` input default in
`.github/actions/rust-toolchain/action.yml` (every CI install step inherits
it; call sites never pass `toolchain:` themselves), and `toolchains.toolchain`
in `MODULE.bazel` (the hermetic Bazel toolchain). `scripts/test_toolchain_pin.py`
fails if they disagree.

Routine bump: after each Rust release, open one PR that moves all three sites
to the new version and fixes whatever the new clippy lints flag. A scheduled
canary run against latest stable (FIG-1684) is the early warning that a bump
is due — a red canary names the offending lints before release day.

The `rust-version` in the workspace manifest is a separate, deliberately
older number: the declared compatibility floor for published crates, not the
toolchain anyone builds with.

## Releases

Merging to `main` does not release. A maintainer manually runs the GitHub
`Release` workflow after selecting a green commit on `main`; leaving
`release_sha` blank selects the current head. The workflow accepts only a
completed, successful full-profile `workflow_dispatch` run whose `headSha`
equals that release commit. A merge-queue run, a neighboring commit, or a
cancelled run is not release evidence.

For the current `main` tip, dispatch `ci.yml` with `--ref main`. To certify an
older commit that is still an ancestor of `origin/main`, use a fresh temporary
branch pinned to that exact commit because GitHub dispatches a branch or tag,
not an arbitrary SHA:

```sh
set -euo pipefail
git fetch origin main
target="$(git rev-parse '<commit>^{commit}')"
git merge-base --is-ancestor "$target" origin/main
cert_branch="release-certification/${target}-$(date -u +%Y%m%dT%H%M%SZ)"
git push --force-with-lease="refs/heads/${cert_branch}:" \
  origin "${target}:refs/heads/${cert_branch}"
gh workflow run ci.yml --ref "$cert_branch"
gh run list --workflow ci.yml --branch "$cert_branch" --event workflow_dispatch \
  --limit 1 --json databaseId,headSha,status,conclusion,url
```

The empty `--force-with-lease` expectation makes branch creation fail if that
name already exists. Before relying on the listed run, verify its `headSha`
equals `target`, then wait for that exact run with
`gh run watch <databaseId> --exit-status`. Keep the branch pinned and dedicated
while it runs. Moving it makes subsequent branch inspection misleading; opening
a pull request from it or dispatching it again creates an event in the same
concurrency group and can visibly cancel the certification run. Delete the
temporary branch after the run completes. Then dispatch `release.yml` with the
same full SHA in `release_sha`; the release workflow independently verifies
that the commit is on `main` and that its exact full-profile CI run succeeded.

After those checks, the release workflow computes the next version, tags the
exact commit, builds assets, and publishes with the auto-generated commit list;
release notes are written manually on the GitHub release afterward.

Never create release tags or publish crates and artifacts by hand. The workflow file is
the executable release contract; this section states the contributor-facing process.
