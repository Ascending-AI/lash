# FIG-1012 implementation report

## Outcome

The `slack-clone` example now exercises `lash-plugin-mcp` end to end. Its bot starts a bundled stdio MCP server, merges the server's two read-only tools into the same catalog as its native Slack tools, and lets an ordinary mention-driven Lash turn call either kind of tool through the standard tool loop.

The bundled tools are:

- `mcp__slack_clone__list_channels_summary`, which returns channel IDs, names, topics, and member counts from the clone's HTTP API.
- `mcp__slack_clone__workspace_stats`, which returns channel, active-member, and channel-membership totals from the clone's HTTP API.

The child process receives the API base URL and bot token through environment variables. It does not receive credentials in argv. `SLACK_CLONE_MCP_SERVER` can override the sibling server-binary path for unusual development layouts.

## Server implementation choice

The demo uses the official `rmcp` server implementation, not a hand-written protocol loop. The workspace's existing `rmcp` dependency exposes the required `server`, `macros`, and `transport-io` features, including the stdio transport, tool router, and server lifecycle. The new `slack-clone-mcp-server` binary serves a `ServerHandler` over `rmcp::transport::stdio()` and calls the clone's real HTTP endpoints through the existing `SlackApi` client.

This keeps the demo protocol-correct and tests the same SDK boundary as the MCP client plugin.

## Connection-pool behavior observed

The integration tests and source inspection established these exact behaviors:

- Lash builds one shared `McpConnectionPool` for the core and eagerly connects configured servers when the MCP plugin is constructed.
- Imported tool definitions remain cached when a transport disconnects, so the catalog stays stable while reconnection runs.
- A call clones the current peer and applies the configured call timeout. `TransportSend`, `TransportClosed`, and `Cancelled` failures mark the server disconnected, cancel the old service, and start background reconnection.
- Reconnection begins after 500 ms and uses exponential backoff capped at 30 seconds. For stdio, reconnecting spawns a new child and rediscovers its tools.
- The killed-server test observes a replacement PID, then proves a subsequent turn succeeds through the respawned server.
- The failed in-flight call is returned as a typed MCP protocol failure and is recorded as a model-visible tool result; it does not hang or panic.
- MCP import names are normalized and made unique within a server, while the server prefix prevents ordinary native-tool collisions. An exact full-name collision across providers is rejected by Lash's registry rather than renamed or shadowed.

Two integration weaknesses were noticed but are outside this example's scope:

- `mark_disconnected` does not record the transport failure in `last_error` before the first reconnect attempt, so status can briefly report a disconnected server with no explanatory last error.
- `CallTimeout` does not mark the peer disconnected. A wedged-but-open peer can therefore remain classified as connected and time out again on the next call.

## Tests added

`examples/slack-clone/tests/mcp.rs` is tokenless and runs against the actual bundled MCP executable plus an ephemeral fake Slack HTTP API. It covers:

1. Catalog integration, an MCP tool call through the standard Lash loop, the result appearing in the next model request, and the result being committed to the transcript.
2. Server death during an in-flight call, bounded completion with a typed failure, no hang or panic, background child respawn, and successful use on the next turn.
3. An exact native/MCP full-name collision, proving registration fails explicitly and the MCP definition is neither shadowed nor silently duplicated.

All test servers bind `127.0.0.1:0`; reserved demo ports 3056 and 3057 were not touched.

## Documentation and production guidance

The example README documents the bundled server, tool names, launch/configuration behavior, and the standard-loop integration. It also makes the production boundary explicit: real deployments should use a remotely hosted streamable-HTTP MCP server and transport credentials in headers, rather than inheriting local stdio environment variables.

## API example-coverage registry

No registry row was changed. The compiler-generated inventory contains no `lash-plugin-mcp` symbols: the standalone plugin crate is outside the registry's current Lash-facade-rooted dependency set. The checker still passes all 7,522 existing entries. This is an honest registry coverage gap; claiming an MCP row as exercised would require first expanding the registry's inventory scope to include the plugin crate.

## Verification battery

All required checks pass on current `origin/main` (`c399fc83`):

- `cargo fmt --all -- --check`
- `cargo check --workspace --all-targets --locked`
- `cargo clippy --workspace --all-targets --locked -- -D warnings`
- `cargo test -p slack-clone --locked` — 61 unit tests and 3 MCP integration tests pass, plus bin/doc-test targets.
- `cargo test -p lash-plugin-mcp --locked` — 14 tests pass.
- `python3 scripts/check_api_example_coverage.py` — 7,522 entries pass.
- `bash scripts/check-production-file-size.sh`
- `prek run --hook-stage pre-commit --files <changed files>`
- `python3 scripts/lint_docs.py` — 46 HTML pages and 42 registry pages pass.
- `git diff --check`

`OPENROUTER_API_KEY` was absent, so the optional short live-model smoke was skipped. The tokenless integration suite still runs the real MCP child process, real stdio protocol, real plugin connection pool, and HTTP-backed demo tools.

## Release note

Release-Notes: Added: the slack-clone bot exercises MCP end-to-end via a bundled demo server (stdio), demonstrating catalog integration, typed failure on server death, and recovery.
