# Contributing to Lash

Feature requests and bug reports are welcome — open an
[issue](https://github.com/Ascending-AI/lash/issues).

At this alpha stage, detailed write-ups help more than drive-by PRs. The
internals are still moving fast, so open an issue before starting a substantial
implementation and agree on the shape first.

To understand how the runtime fits together, start at <https://lash.run/>. The
architecture chapters cover the crate layout, turn/effect boundary, and plugin
model.

## Development workflow

Lash uses trunk-based development. `main` is the only long-lived branch and is
kept releasable.

1. Update `main` and create a short-lived branch.
2. Make one focused change and run the relevant local checks.
3. Open a pull request into `main`.
4. Keep the branch current and merge only after required CI is green.
5. Delete the branch after merge.

Changes to turn execution in `lash-core` or its `lash-restate` adapter must run
both durable geometries locally: `just agent-workbench-restate-e2e` and
`just restate-postgres-workers-e2e`. The latter is required because replay,
ingress, and failover behavior can differ behind the two-worker proxy even when
the single-endpoint workbench is green.

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
- `+40` distributed-worker MinIO;
- `+41..+46` process-operations MinIO, Restate, and PostgreSQL;
- `+47` version-bump recreation PostgreSQL.

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

There is no `staging` branch. Preview work belongs in pull requests, while the
merged product state lives on `main`.

## Releases

Merging to `main` does not release. A maintainer manually runs the GitHub
`Release` workflow after selecting a green commit on `main`; leaving
`release_sha` blank selects the current head. The workflow verifies main-branch
CI, requires curated release notes, computes the next version, tags the exact
commit, builds assets, and publishes.

Never create release tags or publish crates and artifacts by hand. See
`docs/PUBLISHING.md` for the complete release contract.
