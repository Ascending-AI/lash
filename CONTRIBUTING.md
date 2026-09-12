# Contributing to Lash

Feature requests and bug reports are welcome — open an
[issue](https://github.com/Ascending-AI/lash/issues).

At this alpha stage, detailed write-ups help more than drive-by PRs. The
internals are still moving fast, so open an issue before starting a substantial
implementation and agree on the shape first.

To understand how the runtime fits together, start at <https://lash.run/>. The
published guides cover the crate layout, turn/effect boundary, and plugin model.

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

Install the repository's commit hook in each regular checkout with
`prek install --hook-type pre-commit`; new warm forks install it automatically.
The hook formats Rust source, including the enrolled `include!` files. When it
changes a file, the commit stops so you can review the result, stage the files
you intend to commit, and retry. The hook never runs `git add`, so it cannot
silently include unrelated or partially staged changes.

Keep local validation proportional to the change:

- Run cheap formatting and static checks relevant to the files you changed.
- For behavior changes, run the narrowest regression that proves the changed
  behavior. `scripts/fast-test.sh` is an optional broader iteration aid when
  reverse-dependency coverage is useful; high-fan-out crates can still select a
  large part of the workspace.
- Add a targeted live recipe only for a named durability or behavior risk that
  the current CI plan does not exercise. Merely touching `lash-core` or
  `lash-restate` does not require running both durable geometries locally.

For Rust compilation, target analysis, and focused unit or integration tests,
use the checkout-independent Bazel workflow in
[`docs/agents/hermetic-build.md`](docs/agents/hermetic-build.md). Its default
entry point uses the shared local executor and cache; the named Cargo recipes
retain feature-matrix, service, doctest, trybuild, fuzz, judged, packaging, and
release semantics.

`just push-gate` and the `just confidence*` lanes remain available as explicit
full diagnostics before an unusual-risk change, release work, or when a user
requests them. They are not routine push or merge prerequisites. Stop once the
focused evidence is green; CI and independent review supply the broad merge
proof rather than repeating the same broad suite locally.

### Required checks and the merge queue

`ci.yml` subscribes to `merge_group`, so a queued pull request is validated from
its own `gh-readonly-queue/main/pr-<n>-<sha>` ref. Its plan job classifies the
exact diff and selects the correctness families configured for that event,
including workspace tests, lint and repository gates, public API checks,
feature checks, confidence shards, store backends, functional E2E, and worker
E2E. The single `CI conclusion` job rejects failed, cancelled, missing, or
incorrectly skipped correctness jobs and is the aggregate merge context.
The separate `Release cache` workflow warms the cache consumed by release.yml
and perf.yml on trusted `main` pushes. It is independently serialized so its
non-gating release build cannot hold required CI or the next main push behind
it; workflow dispatch supplies the manual recovery path.

The workers E2E family runs when selected on `main` pushes and full-profile
(`workflow_dispatch`) runs, and on pull requests carrying the `ci:workers`
label. It does not execute in the merge queue. Run a local worker recipe only
when a changed behavior needs earlier evidence or falls outside that CI
coverage; name that risk and recipe in the PR.

Pull requests land through the `main` merge queue, which revalidates every entry
against the true merged base before it lands — never by direct merge. Keep the
ruleset's required context aligned with the aggregate `CI conclusion`; renaming
that job without updating the ruleset will wedge every queue entry behind a
check that can never report.

## Concurrent local gates

`just push-gate`, the `just confidence*` batteries, and their container-backed
E2E recipes are isolated by worktree. They derive a stable slug by lowercasing
the basename of the script's physical worktree root, replacing non-alphanumeric
runs with `-`, trimming leading or trailing `-`, and appending the first eight
hex digits of a stable checksum of the absolute worktree path. Thus two
checkouts with the same basename still have distinct identities. Container
names, fixed Compose projects, persistent external network names, default
evidence paths, and default host ports all include or derive from that slug.

Each absolute worktree path hashes with `cksum` into one of 90 disjoint 50-port
blocks spanning 61000–65499, above Linux's default ephemeral range. The lane
offsets are stable:

- `+0..+9` attachment/usage workbench PostgreSQL, selected by the workbench
  port's last decimal digit;
- `+10` push/confidence PostgreSQL, `+11` push MinIO, `+12` mutation PostgreSQL;
- `+20..+23` agent-service Restate and endpoint;
- `+30..+34` agent-workbench Restate, endpoint, and PostgreSQL;
- `+35..+37` slack-clone full-host platform, bot, and HTTP MCP server;
- `+40` distributed-worker MinIO;
- `+41..+46` process-operations MinIO, Restate, and PostgreSQL;
- `+47` version-bump recreation PostgreSQL.
- `+48` slack-clone live-model platform.

Explicit existing environment overrides such as `LASH_PUSH_GATE_PORT_BASE`,
`LASH_PUSH_GATE_POSTGRES_PORT`, `LASH_CONFIDENCE_OUT_DIR`, and each recipe's
named port/container/artifact variables remain authoritative escape hatches.
If two concurrently active worktrees select the same block, set
`LASH_GATE_SLOT_OVERRIDE` to an unused integer from `0` through `89` for one
gate; this changes its derived port base while preserving its path-qualified
ownership identity. The refusal prints this override and the occupied lock
path.
The default confidence evidence root is
`target/confidence/<worktree-slug>/` for local runs. CI explicitly pins
`LASH_CONFIDENCE_OUT_DIR` to `target/confidence` so its established artifact
upload and summary paths are unchanged.

Every worktree uses a fixed external network named `lash-e2e-<worktree-slug>`.
Scripts create it idempotently and never delete it, because host network
watchers treat Docker network add/remove as interface churn. Compose projects
are fixed per worktree rather than per run. Their repeated `postgres`, `minio`,
and `restate` aliases are safe only because the worktree lock and labeled
leftover check prevent two lane projects from sharing this network at once. A
nonblocking worktree lock rejects a second same-worktree battery with exit 73.
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
LASH_GATE_WORKTREE_SLUG=legacy LASH_E2E_MINIO_PORT=1 \
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
just gate-worktree-concurrency-check /path/to/peer-worktree
```

The check runs PostgreSQL, MinIO, and Restate smokes concurrently in both
worktrees, then proves a second same-worktree run refuses cleanly. Evidence is
written below `target/gate-concurrency-proof/<worktree-slug>/` unless
`LASH_GATE_PROOF_OUT_DIR` overrides it.

### Machine load, as distinct from gate isolation

Worktree isolation makes concurrent gates *correct*; it does nothing about the
machine they share. Every concurrent `just push-gate` compiles the whole
workspace, and an unbudgeted build sizes itself from `nproc`, so several gates
at once oversubscribe the box and each one finishes later than it would have by
waiting. Two limits, both feature-detected and both absent on CI runners, which
get a runner per job and have nothing to share:

- **How wide one gate goes.** The build width comes from the environment —
  `CARGO_BUILD_JOBS` and `NEXTEST_TEST_THREADS`, exported by whatever prepares
  the checkout — not from `nproc`.
- **How many gates run at once.** `push-gate.sh` runs its build-heavy legs
  (workspace check, clippy, the workspace test build, the doc passes) through
  `heavy-slot` when that tool is on `PATH`: a box-wide semaphore that caps how
  many compile-shaped gates are resident at once and *waits* for a slot rather
  than failing. Absent, the legs run exactly as before.

Raise a budget or bypass the semaphore only for a specific measured need. An
unbudgeted gate does not finish sooner; it makes every other one finish later.

There is no `staging` branch. Preview work belongs in pull requests, while the
merged product state lives on `main`.

## Releases

Merging to `main` does not release. A maintainer manually runs the GitHub
`Release` workflow after selecting a green commit on `main`; leaving
`release_sha` blank selects the current head. The workflow accepts only a
completed, successful full-profile `workflow_dispatch` run whose `headSha`
equals that release commit. A successful push, merge-queue run, neighboring
commit, or cancelled run is not release evidence.

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
