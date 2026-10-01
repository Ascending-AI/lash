# Lash Buck2 tooling

Use `kiln build`, `kiln check`, `kiln test`, `kiln clippy`, `kiln doc` and
`kiln run` after creating your own Kiln fork and sourcing `./env.sh`.
`scripts/hermetic-build.sh` is the repository entrypoint. The driver uses the
official, unmodified Buck2 executable pinned in `pins.json`; bootstrap verifies
both the archive and executable checksums and installs only ignored files, in
the checkout and in the [shared bootstrap store](#shared-bootstrap-store).

[Developer workflows](../../docs/agents/hermetic-build.md) describe target
selection, feature lanes, profiles, service gates, generated files, materialized
outputs and supported Cargo recipes. [Migration evidence](../../docs/agents/buck2-migration.md)
records NativeLink acceptance, parity checks, measurements and deployment order.
Cargo manifests and the workspace lockfile remain authoritative.
`kiln check` selects native Rust metadata outputs; `kiln build` produces full
libraries and executables. Use `check` for the shortest compiler feedback loop.

## Deployment configuration

Kiln generates ignored `.buckconfig.local` from the existing executor manifest.
Endpoints, TLS identity, instance name and runtime fingerprint are deployment
settings and must not be committed. The combined client certificate/key PEM is
private, mode 0600, under `.kiln/`. Buck2 endpoint values omit the `grpcs://`
scheme. The graph uses SHA256 digests and the existing NativeLink pool.

The driver defaults to 16 remote actions, or 32 in CI. `--jobs N` selects remote
concurrency; eight coordinator threads bound local scheduling. Compatibility
and benchmark runs use controlled concurrency separately from these defaults.
Each checkout has its own `kiln` isolation directory and daemon.

## Shared bootstrap store

A fresh checkout needs about 1.8 GiB of pinned inputs. `bootstrap_store.py`
keeps one verified copy of each for every checkout of this user:

| Entry | Named by | In the checkout |
| --- | --- | --- |
| `buck2` | archive and executable checksums in `pins.json` | `.buck2/bin/buck2`, cloned |
| `prelude` | Buck2 executable checksum and `prelude_overlay.py` | `.buck2/prelude`, cloned; its receipt is written locally |
| `rust` | `toolchain-lock.json` | `tools/buck2/toolchains/rust-files`, cloned |
| `native-<tool>` | the tool's archive record in `native-tools-lock.json` | `.buck2/native/<tool>`, cloned; `BUCK` and the receipt are written locally |
| `reindeer` | the asset in `reindeer-lock.json` | `tools/buck2/bin/reindeer`, cloned |
| `vendor` | the checksums `bootstrap_vendor.py` already records | `vendor`, a symlink to the entry |

The store is `$LASH_BUCK2_STORE`, else `$XDG_CACHE_HOME/lash-buck2`, else
`~/.cache/lash-buck2`. `LASH_BUCK2_STORE=off` disables it, and CI leaves it off
unless the variable names a directory. The store must be owned by the current
user and writable by no one else. If it is not usable, each script installs
into the checkout exactly as it did before the store existed.

A clone is a reflink where the filesystem has them, otherwise a hardlink,
otherwise a copy, so put the store on the checkouts' filesystem to share disk.
Buck2 does not read source trees through a symlink that leaves the project, and
would hash such a link as an absolute path, so every action input is cloned.
`vendor` is not an input and `.buckconfig` ignores it, so the file watcher
never follows that link. Each stored inode gains one hardlink per live
checkout; past 1,000 links, or at the filesystem's limit, the file is copied
instead. Stored and cloned files are read-only because a hardlink shares its
bytes with every other checkout: write a new file and rename it over the old
one, as `prelude_overlay.py` does.

An entry is built in a private directory by the script that owns its pin,
which verifies the pinned checksums, and is published by one rename under a
per-entry lock. Concurrent checkouts wait and then clone the same entry. Every
clone first compares the entry with its manifest (paths, sizes, modes,
modification times and link targets); an entry that differs is rebuilt, never
used. `python3 tools/buck2/bootstrap.py --verify-store` rehashes every file
and deletes the entries that fail.

Each checkout records the entries it uses. `python3 tools/buck2/bootstrap.py
--prune-store` deletes the entries that no existing checkout references and
that nothing used in the last day (`--unused-for SECONDS` changes the period),
with stale staging directories and unused graph receipts. It skips an entry
another process holds. A checkout whose `vendor` entry was deleted vendors
again on its next command.

`sync.py --check` also files its receipt in the store, keyed by the receipt's
inputs. A checkout with no current receipt adopts one written for identical
inputs and verifies it against its own files before trusting it.

## Action resources and results

Each action's canonical budget supplies the platform properties `cpu_count`,
`memory_kb`, `cpu_arch`, `OSFamily` and `kiln_executor_runtime`, plus outer-command
`KILN_ACTION_CPU_COUNT` and `KILN_ACTION_MEMORY_KB`. The latter reaches the
existing worker supervisor before compiler wrappers run. Compile and test
requests use their respective sizing tables, which
`action_sizes_from_log.py --refresh` rebuilds from the workers' usage logs; see
[execution and resource accounting](../../docs/agents/hermetic-build.md#execution-and-resource-accounting)
for the rule and the refresh. The checksum-pinned Starlark
prelude overlay attaches this environment through the documented action API;
it does not modify or rebuild the Buck2 executable.

All actions stay remote: the shared platforms are remote-only. The overlay
replaces the prelude's remote `failure_filter` round trip with a daemon-side
decision read from the compile's build status. A passing compile's output is a
declared copy; a failing compile still runs the stock remote action and reports
the same error. See [local and remote actions](../../docs/agents/hermetic-build.md#execution-and-resource-accounting),
which also covers what `[project] ignore` does and does not do for the file
watcher.

The external test runner uses Buck2's Execute2 API. Its Python wheels and
upstream protocol definitions are checksum-pinned and private to the checkout.
`--test-report PATH` records actual verdicts and action/cache metadata.
`--test-output-dir DIR` contains `<cell>/<package>/<target>/test.xml`, `test.log`
and the complete `undeclared` receipt tree. Without those flags each invocation
writes under `.buck2/test-invocations/<id>/` and links the default paths to it
when it ends. Each test stage carries a variant derived from its selection, so
[concurrent invocations](../../docs/agents/hermetic-build.md#concurrent-invocations)
of one target never share Buck2's declared-output directory, and a report that
does not match its selection is an infrastructure failure. Timeouts,
cancellation and malformed reports preserve failure evidence. Service inputs force local uncached test
execution while compilation remains remote and cacheable.

Callers that execute build outputs request `--materializations final` and use
`outputs.py --report PATH --label //package:target --single`; do not guess an
output path. The driver translates `final` to the stock client's `all` mode
for requested outputs. `run` preserves the invocation's `BUILD_WORKING_DIRECTORY` and the
repository's `BUILD_WORKSPACE_DIRECTORY`.

Run focused tooling checks with:

```sh
python3 -m unittest discover -s tools/buck2/tests -v
```

Primary contracts are Buck2's [action environment API](https://buck2.build/docs/api/build/AnalysisActions/),
[executor configuration API](https://buck2.build/docs/api/build/CommandExecutorConfig/)
and [external test runner interface](https://buck2.build/docs/rule_authors/test_execution/),
plus the pinned [NativeLink environment implementation](https://github.com/TraceMachina/nativelink/blob/0d5f173fd39edbf5b284e550aede94c60e93479a/nativelink-worker/src/running_actions_manager.rs#L1855).
