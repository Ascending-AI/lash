# Lash patch to rmcp 3.5.1

Allow `x-mcp-header` annotations reached solely through `properties`, mirroring
values at their exact argument paths on the HTTP client and validating the same
paths on the server. Annotations reached through any other keyword invalidate
the tool, while existing token, type, uniqueness and value encoding rules remain.

Upstream issue: https://github.com/modelcontextprotocol/rust-sdk/issues/1354

Delete this patch and the root `[patch.crates-io]` entry once a released rmcp
contains the fix.
