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
the basename of `git rev-parse --show-toplevel`, replacing non-alphanumeric
runs with `-`, and trimming leading or trailing `-`. Container names, fixed
Compose projects, persistent external network names, default evidence paths,
and default host ports all include or derive from that slug.

Each slug hashes with `cksum` into one of 64 disjoint 64-port blocks spanning
61000–65095, above Linux's default ephemeral range. The lane offsets are stable:

- `+10` push/confidence PostgreSQL, `+11` push MinIO, `+12` mutation PostgreSQL;
- `+20..+23` agent-service Restate and endpoint;
- `+30..+34` agent-workbench Restate, endpoint, and PostgreSQL;
- `+40` distributed-worker MinIO; and
- `+41..+46` process-operations MinIO, Restate, and PostgreSQL; and
- `+47` version-bump recreation PostgreSQL.

Explicit existing environment overrides such as `LASH_PUSH_GATE_PORT_BASE`,
`LASH_PUSH_GATE_POSTGRES_PORT`, `LASH_CONFIDENCE_OUT_DIR`, and each recipe's
named port/container/artifact variables remain authoritative escape hatches.
The default confidence evidence root is
`target/confidence/<worktree-slug>/`.

Every worktree uses a fixed external network named `lash-e2e-<worktree-slug>`.
Scripts create it idempotently and never delete it, because host network
watchers treat Docker network add/remove as interface churn. Compose projects
are fixed per worktree rather than per run. A nonblocking worktree lock rejects
a second same-worktree battery with exit 73, and labeled leftover containers
produce a refusal that names the exact `docker rm -f` recovery command. A hash
slot lock turns the unlikely case of two slugs selecting the same block into a
clean refusal rather than a host-port race.

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
