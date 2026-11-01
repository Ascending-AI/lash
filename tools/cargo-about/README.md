# Release license notices

`bootstrap.py` pins cargo-about 0.9.3 and its archive checksum. The worker
packager and dependency-policy workflow call `scripts/generate_notices.py`.
Its temporary cargo-about configuration comes from `deny.toml`, including
crate-scoped exceptions and the confidence threshold. No second allowlist or
generated notices are checked in.

Cargo's locked `tree --edges normal` selects the Linux worker's exact closure
with the release dialect features. cargo-about gathers the workspace's license
records, then the generator selects exactly those components and checks their
coverage. This avoids cargo-about's graph filtering losing feature edges.
Each component carries its authors, source and complete selected license texts.
The generator also preserves the original license, copyright and NOTICE files
provided by the locked sources. Where a crate omits its license file or the
scanner cannot identify it, cargo-about supplies the canonical SPDX text.
Unacceptable licenses, unknown expressions and missing license texts fail.

Crate dependencies ship by reference. `package_workspace.py` and
`publish_workspace.py` generate `THIRD-PARTY-NOTICES` only for published crates
that bundle license files, including vendored sources and test data. These
files are regenerated before every package/publish, and Cargo's existing
`--allow-dirty` includes them. They are not committed. The dependency job also
runs this generation.

To inspect the notices in a sourced Kiln fork:

```sh
kiln gate lash <fork> -- python3 scripts/generate_notices.py --out /tmp/THIRD-PARTY-NOTICES --published
```

`package_vm_worker.py` always regenerates the worker file and includes it in
the archive with its SHA-256 in `manifest.json`. A generation error stops
packaging before the archive is opened.
