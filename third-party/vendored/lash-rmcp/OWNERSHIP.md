# Lash-owned MCP SDK

`lash-rmcp` lives under `third-party/vendored/` so the complete upstream crate
stays outside Lash's source lint, formatting, wording and file-size policy.
It is a publishable workspace member, versioned and released alongside Lash.
The library name and consumers' dependency alias remain `rmcp`; the unchanged
crates.io `rmcp-macros` generates `rmcp::` paths and needs no fork.

Upstream base: rmcp 3.5.1, rust-sdk commit
`79437f291b2c44053d00dcd5db969fd0cca7c887`. The Apache-2.0 license and original
attribution are retained. `Cargo.toml.orig` is the pristine upstream manifest.
`rmcp.diff` records changes to the upstream manifest, README and header module
against that release. Nested `x-mcp-header`
annotations reached solely through `properties` are validated and mirrored at
their exact argument paths. Annotations reached through other keywords invalidate
the tool. See https://github.com/modelcontextprotocol/rust-sdk/issues/1354.

To merge an upstream release, unpack its entire crate, merge `rmcp.diff`, and
retain the Lash package identity, workspace version, library name, ownership
note and license/notice files. Keep all upstream tests and supporting files.
Refresh `rmcp.diff` against the pristine release, update the upstream base here,
then run `kiln sync` and the header-path laws. Upstream's Cargo.lock is retained
for comparison; the workspace Cargo.lock governs Lash builds and releases.
