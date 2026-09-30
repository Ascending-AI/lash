# Admission control lives in provider decorators

## Status

accepted

## Decision

Admission windows, priorities, circuit breakers and shared backpressure metrics are host policy. Hosts install a decorator through `ProviderComponents::map_provider`. `Provider` is dyn-compatible and cloneable through `clone_boxed`; the installed decorator receives each retry attempt's full `LlmRequest`. Every request scope carries a session id.

The runtime owns per-handle reliability: timeouts, retry accounting and local rate limits. Cross-handle fairness, vendor quota allocation, spend policy and tenant classes stay with the host under ADR 0014.

## Rules and guarantees

Retryable quota failures with `Retry-After` can defer without consuming ordinary attempts. Courtesy is bounded by cumulative `throttle_wait_budget_ms`, default 90 seconds, and eight courtesy calls. Zero disables the wait budget. After either bound is exhausted, throttles consume the ordinary retry ladder. Total calls are bounded by courtesy calls plus `max_attempts`.

Decorators forward `close`, keep awaits cancellation-safe and re-wrap each construction site. Serialized provider configuration contains the underlying provider, so reconstruction does not recreate host wrapper state. Direct completions accept a host session id and otherwise use a fresh `direct:{uuid}` identity. Hosts can assign their own traffic classes from those ids.

## Alternatives and consequences

A core `AdmissionController` with a shipped policy is rejected because admission spans handles, sessions and processes while core dispatch is per handle. A host dispatch-class field or general metadata bag on the request is rejected because it merely transports host vocabulary back to the host. Session identity already supplies the classification key.

Hosts own decorator metrics and policy. Core retry handling remains responsible for honoring bounded throttling so a wrapper does not need to compensate for incorrect retry exhaustion. [Provider composition](../../crates/lash-core-llm/src/provider/handle.rs), [retry accounting](../../crates/lash-core-llm/src/provider/options.rs) and [direct requests](../../crates/lash-core-execution/src/direct.rs) implement the contract.
