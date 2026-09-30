# Process waits live on the work-driver seam

## Status

accepted

## Decision

`ProcessRegistry` stores process state, observer relationships, wake bookkeeping and events. It exposes point reads and mutations rather than backend-specific wait methods. Coordination belongs to `ProcessWorkSubstrate::await_process_terminal`, returning `ProcessTerminalWait::{Terminal, Reattach}`.

`ProcessRegistryAwaiter` reads `get_process` and `event_page`. A watched registry's `ProcessChangeHub` supplies local notifications; backoff handles remote mutations. Default polling starts at 25 milliseconds, doubles, and caps at one second. `NoProcessWork` uses this awaiter for a backend that executes no processes. Restate executes processes and owns its terminal wait through ingress to `LashProcessWorkflow/{process_id}/await_terminal`.

## Terminal-wait failures

Transient ingress errors return `Reattach` without writing a process outcome. Connection refusal or reset, response-read EOF, truncated JSON, timeout, HTTP 408/429 and ingress-generated 5xx failures are transient. Reattachment observes the same durable process. Definitive targets, typed refusals, local encoding errors and complete malformed responses remain errors. An invocation-sourced failure stays definitive even when its HTTP status is 5xx. Cancel watches and bounded sends use the same classifier; retries retain workflow identity, payload and any send idempotency key.

`CallerDeparted` refuses an unresolved wait before contacting ingress. A retained terminal outcome is returned from durable state. Engine errors do not fall back silently to database polling.

## Why and consequences

Per-backend wait loops duplicate lost-wakeup handling and force stores to know the deployment's execution economics. They are rejected. The work driver chooses durable engine suspension or shared point-read coordination. Watched decorators can add local notifications without changing storage traits, and sinks under ADR 0017 remain optional freshness rather than terminal authority.

[Shared awaiter](../../crates/lash-core-execution/src/runtime/work/awaiter.rs), [cadence](../../crates/lash-core-execution/src/runtime/work/cadence.rs), [Restate process ingress](../../crates/lash-restate/src/process/mod.rs) and [HTTP failure classification](../../crates/lash-restate/src/ingress.rs) implement the decision.
