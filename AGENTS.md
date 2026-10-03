# Working in Lash

Use one isolated Kiln fork per agent or reviewer:

```sh
fork="$(kiln fork lash my-change)"
cd "$fork"
. ./env.sh
```

Use `kiln build`, `kiln check`, `kiln test`, `kiln clippy` and `kiln analyze`
for development. Lash uses the checksum-pinned official Buck2 executable and
the existing NativeLink pool. `kiln test` runs the cacheable developer partition;
it does not run every correctness gate. While editing, prefer an owning target
and `--test_arg=<filter>`.

The proof a change needs is minimal and fast; main's hourly full run covers
the rest and reds are fixed forward:

- the tests the change adds or changes, run once on the cheapest tier
  (SQLite stores, the in-process Restate server double), by full test path;
- for a bug, a law that fails once on the unfixed code;
- no other new tests: a test must pin a named rule or a found bug
  (coverage padding and change detectors are rejected in review, see
  [way of working](docs/agents/way-of-working.md#which-tests-get-admitted));
- one `kiln clippy`;
- only when they apply: `//crates/lash:ui_fixtures` and
  `//crates/lash:facade_completeness` when exports change, and
  `kiln build //:schema_checks` when a serialized shape changes.

`python3 scripts/dev-test.py --dependents` is an optional local tool for a
wider affected-tests selection, not a required gate. See [the build
guide](docs/agents/hermetic-build.md) for partitions, reports, features and
service gates.
Prefer `kiln check` for compiler feedback: it checks metadata without linking.
Use `kiln build` when you need full libraries or executables.

Cargo manifests and `Cargo.lock` remain canonical. Preserve package identities,
features and consumer compatibility. After changing dependencies or target
membership, run `kiln sync` and review the generated BUCK files and inventory;
normal builds reject generated-file drift.

Keep named Cargo recipes for publishing, release artifacts, nested-Cargo tests,
feature witnesses and other Cargo-owned gates. Source `env.sh` before **any**
Cargo command. Cargo on PATH is Kiln's shim and already supplies target paths,
flags and admission. Never set `CARGO_*` manually or change job budgets without
measurements. Local Cargo validation should retain
`--workspace --all-targets --locked`; package-only checks alter the workspace
feature graph.

Use `kiln gate lash <fork> -- <command>` for integration gates. Use
`KILN_GATE_ID` for private service identities and ports. Do not run development
selection against live PostgreSQL/S3 settings. Never write under `golden-*`,
share an active implementation/review worktree, stop other agents' builds, or
delete forks manually. Remove your fork with `kiln rm lash <name>` after merge.

Keep credentials and host configuration out of Git. Kiln generates private
`.buckconfig.local` and certificate state from executor metadata. Preserve all
five execution properties and the separate per-action compile/test budgets.
Do not substitute empty platform properties or a uniform budget.

One lead owns decisions; persistent sidekicks own execution. Exchange concise
briefs and evidence. Make review independent, and report a gate that could not
run instead of claiming it passed.
