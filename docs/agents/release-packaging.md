# Release packaging

The release workflow stamps `scripts/release_version.py print-next` into its
checkout before packaging. Main keeps the `0.0.0-dev` placeholder. Do not commit
a stamped version or push a release tag during local qualification.

`package_workspace.py --version VERSION --no-verify` packages every publishable
workspace member selected by Cargo metadata in one Cargo invocation. Every
surviving workspace dependency must be publishable and exactly pinned to the
same version. Unversioned development-only path dependencies are stripped by
Cargo; use those for test-support edges that would create publish cycles.
Packaging failures are fatal. Without `--no-verify`, Cargo also builds the
packages from their extracted contents. The workflow additionally uses
`--verify-package lash-kernel-engine` to build that standalone crate with its
dialect features from the normalized `.crate` contents and packaged siblings.
Those packages replace registry dependencies only for this pre-upload proof;
no checkout source path participates. `publish_workspace.py --plan` prints
the same derived graph in dependency layers without publishing. Its upload
path checks downloaded crate digests against Cargo's local package.

The worker job has Python/TypeScript, TypeScript-only and Python-only matrix
rows. After stamping and `kiln sync`, select and build each row through its
compiled feature set, for example from a sourced Kiln fork:

```sh
dialects=python,typescript
worker_label="$(python3 scripts/package_vm_worker.py --print-target --dialects "$dialects")"
kiln build --config=optimized --materializations=final \
  --build-report .kiln/worker-build.json "$worker_label"
worker="$(python3 tools/buck2/outputs.py --report .kiln/worker-build.json \
  --label "$worker_label" --single)"
kiln gate lash FORK -- python3 scripts/package_vm_worker.py \
  --worker "$worker" --dialects "$dialects" --out .kiln/worker-artifacts --version VERSION
```

Each `lash-sdk-worker-VERSION-linux-x86_64-DIALECTS.tar.gz` contains the stripped
`bin/lash-vm-worker`, `THIRD-PARTY-NOTICES` from its own locked normal dependency
closure and `manifest.json`. `DIALECTS` is `python-typescript`, `typescript` or
`python`. An external `.sha256` checks the archive. No source tree is bundled.
The manifest records `version`, `target`, `dialects`, `protocol_version`,
`minimum_supported_protocol_version`, `glibc_minimum` and the `binary`/`notices`
objects, each with `path` and `sha256`. Linux releases declare glibc 2.34;
packaging refuses newer imported glibc symbol versions. Release notes repeat
this floor. GitHub's source archives and Cargo packages provide source access.

The `cargo-deny` check runs on every PR and push to main, plus a weekly advisory
scan. In GitHub's main branch rule, require the `cargo-deny` status check before
merging. That hosted setting is not managed by this repository.
