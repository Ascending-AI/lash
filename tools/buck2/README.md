# Lash Buck2 tooling

Use `kiln build`, `kiln check`, `kiln test`, `kiln clippy`, `kiln doc` and
`kiln run` after creating your own Kiln fork and sourcing `./env.sh`.
`scripts/hermetic-build.sh` is the repository entrypoint. The driver uses the
official, unmodified Buck2 executable pinned in `pins.json`; bootstrap verifies
both the archive and executable checksums and installs only checkout-private
ignored files.

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

## Action resources and results

Each action's canonical budget supplies the platform properties `cpu_count`,
`memory_kb`, `cpu_arch`, `OSFamily` and `kiln_executor_runtime`, plus outer-command
`KILN_ACTION_CPU_COUNT` and `KILN_ACTION_MEMORY_KB`. The latter reaches the
existing worker supervisor before compiler wrappers run. Compile and test
requests use their respective sizing tables. The checksum-pinned Starlark
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
and the complete `undeclared` receipt tree. Timeouts, cancellation and malformed
reports preserve failure evidence. Service inputs force local uncached test
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
