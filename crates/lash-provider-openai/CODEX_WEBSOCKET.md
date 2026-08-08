# Codex WebSocket Notes

The Codex provider owns a provider-local WebSocket session cache. The cache is
bounded by `MAX_SESSION_WEBSOCKET_CACHE_ENTRIES` and pruned by idle TTL; the idle
prune closes sockets by dropping the stream (a TCP-level close).

## Shutdown Hook

The `Provider` contract exposes `async fn close(&self)` (defaulting to a no-op),
forwarded by `ProviderHandle::close`. `CodexProvider` implements it by draining
the WebSocket session cache and sending a real WebSocket Close frame on every
idle cached connection before dropping it, rather than relying on the bare TCP
drop that a provider `Drop` would give. Busy entries leased to an in-flight
`complete` call are not held in the cache, so `close` touches only idle reusable
sessions; the lease closes or re-caches its own socket on release. The cache is
shared across `CodexProvider`/`ProviderHandle` clones (an `Arc`), so a host that
retained a clone can close it from its shutdown path to release the sockets the
running handle cached. Hosts call this before process exit; it is the graceful
counterpart to the idle-prune's synchronous drop.

Known design concerns to keep explicit:

- Lash attaches `LlmRequestScope` at every provider request boundary.
  Runtime LLM calls include `session_id`, `agent_frame_id`, and `request_id`.
  Direct calls without a user session get an explicit generated direct scope.
- The provider uses `session_id` for Codex gateway affinity, `request_id` for
  request correlation, and `session_id + agent_frame_id` as its local
  continuation/cache key. A frame switch must not inherit another frame's
  `previous_response_id`.
- The provider can only send a cached WebSocket delta when the next full request
  starts with the previous request input plus previous response output. Runtime
  turn-local prompt insertions can break that prefix and force a full-context
  request.
- Auto transport records a short per-scope WebSocket fallback cooldown after
  pre-output transport failures. That avoids repeatedly hitting a known-bad
  WebSocket path without turning an explicit `websocket` or `websocket_cached`
  user choice into SSE.

## Minimal Core Continuation Contract

Core does not expose provider-specific conversation state. It supplies request
scope only, and scope is always present on provider requests:

- `session_id`: logical Lash session.
- `agent_frame_id`: durable frame/branch within that session.
- `request_id`: one provider call, suitable for correlation/idempotency.

The Codex provider owns the WebSocket mechanics. It may reuse a cached
`previous_response_id` only when the current projected request has the same
non-input fingerprint and its input prefix exactly matches the prior request
input plus prior response output. Otherwise it sends the full context and records
the explicit cache miss reason, such as `input_prefix_mismatch`.

## Runtime Test Seam

Codex exposes two constructor-level injection seams, both in the same spirit
as `CodexProvider::with_http_transport`: explicit builder calls on an
in-process provider instance, never env vars, and never serialized into user
config (`CodexProviderFactory` always rebuilds with the production endpoints
and the default HTTP transport).

- `CodexProvider::with_endpoint_urls(responses_url, websocket_url)` points the
  provider at alternative Responses HTTP and WebSocket endpoints, e.g. local
  scripted servers.
- `CodexProvider::force_websocket_transport()` pins the WebSocket path
  (the counterpart of `force_sse_transport`), so a test exercises it
  deterministically instead of relying on `Auto`'s try-then-fall-back.

The scripted WebSocket server harness lives in `codex::ws_testing` (compiled
for unit tests and behind the default-on `testing` feature). The provider-layer
unit tests in `src/codex.rs` drive `CodexProvider` against it directly; the
runtime-level tests in `tests/codex_websocket_runtime.rs` build a normal
facade (`LashCore::standard_builder(TurnBudget::Unbounded)` + `ProviderHandle`) around a provider
configured through the seams above and prove full turns over the WebSocket
transport: streamed assistant text end-to-end, and a tool-call turn whose
`function_call_output` round-trips on the follow-up request.
