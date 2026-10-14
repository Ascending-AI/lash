# Lash-owned MCP SDK

`lash-rmcp` lives under `third-party/vendored/` so the complete upstream crate
stays outside Lash's source lint, formatting, wording and file-size policy.
It is a publishable workspace member, versioned and released alongside Lash.
The library name and consumers' dependency alias remain `rmcp`; the unchanged
crates.io `rmcp-macros` generates `rmcp::` paths and needs no fork.

Upstream base: rmcp 3.5.1, rust-sdk commit
`79437f291b2c44053d00dcd5db969fd0cca7c887`. The Apache-2.0 license and original
attribution are retained. `Cargo.toml.orig` is the pristine upstream manifest.
`rmcp.diff` records changes to the upstream manifest, README, fixture paths and
header module against that release. Upstream's `build.rs` is deliberately
dropped: it configured `core.hooksPath` two directories up, and a dependency
must not install repository hooks. The manifest's `build` key goes with it.
Nested `x-mcp-header`
annotations reached solely through `properties` are validated and mirrored at
their exact argument paths. Annotations reached through other keywords invalidate
the tool. See https://github.com/modelcontextprotocol/rust-sdk/issues/1354.

Tests locate fixtures through `CARGO_MANIFEST_DIR`; Buck2 declares each fixture
as a resource of its owning test. The SDK's dev dependencies do not enable
`serde_json/arbitrary_precision`: workspace feature unification would change
Lash's stored MessagePack number representation and canonical definition ids.
The SDK still accepts decimal fields when a consumer enables that feature.
The upstream `arbitrary_precision::feature_is_enabled` setup sentinel is
deliberately deleted with that dev-only edge. Keep both divergences on upstream
merges: `definition_id_golden_vectors_are_frozen` pins Lash's numeric profile,
and every SDK message-decoding behaviour law is retained.

To merge an upstream release, unpack its entire crate, merge `rmcp.diff`, and
retain the Lash package identity, workspace version, library name, ownership
note, license/notice files and the dropped hook-installing `build.rs`. Keep all
upstream behaviour tests and supporting files.
Refresh `rmcp.diff` against the pristine release, update the upstream base here,
then run `kiln sync` and the header-path laws. Upstream's Cargo.lock is retained
for comparison; the workspace Cargo.lock governs Lash builds and releases.
