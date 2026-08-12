# Admission Control Lives in Provider Decorators

## Status

accepted

## Decision

Lash ships no admission-control machinery in the LLM dispatch path. Admission
windows, priority lanes, circuit breakers, and backpressure metrics are host
policy, and they enter by wrapping the provider the host installs: the
`Provider` trait is deliberately dyn-compatible (`Box<dyn Provider>`,
`clone_boxed`) and decorator-friendly, and `ProviderComponents::map_provider`
is the sanctioned install point. A decorator's `complete()` sees every
provider call — including each attempt of the retry ladder — with the full
`LlmRequest`, whose `scope.session_id` is contractually present on every
request, so hosts class traffic by session identity they already own. The
runtime keeps only the per-handle knobs it needs to be a well-behaved client
of one backend (`ProviderReliability`: timeouts, the retry ladder, local rate
limits); everything cross-cutting — fairness between tenants, load shedding,
breaker state shared across handles — stays outside.

## Why

Admission control is operational policy in the sense of ADR-0014: the host
owns the resources being protected (vendor quotas, spend budgets, tenant
fairness) and only the host knows its traffic classes. A lash-shipped
admission layer would need exactly the configuration surface the host already
has in its own vocabulary, and would sit at the wrong scope — admission
usually spans provider handles, sessions, and often processes, while lash's
dispatch path is deliberately per-handle. The decorator seam already exists,
is proven by the transport-metrics wrappers in the provider tests, and
requires no new contract: wrapping is composition over the same `Provider`
trait every backend implements. What lash must guarantee is that the seam is
honest — that the pieces a wrapper cannot reach behave correctly on their own.
That is why the retry ladder had to learn to treat provider throttling as
deference rather than failure (waiting out `Retry-After` without burning
attempts, bounded by `throttle_wait_budget_ms`): retries wrap the decorator,
so an admission gate cannot compensate for a ladder that turns a 429 storm
into attempt exhaustion.

## Consequences

- Hosts that need admission control write a small decorator (the providers
  page documents the pattern and its disciplines: forward `close()`
  explicitly, re-wrap at every construction site because `serialize_config`
  round-trips only the inner provider's config, keep admission awaits
  cancellation-safe, class by `scope.session_id`).
- Traffic classing needs no new request field: hosts mint deliberate session
  ids for their own pipelines — including direct completions, which accept a
  host-supplied session id and otherwise scope as `direct:{uuid}` — and apply
  a default-lane policy to ids they did not mint (lash-spawned child
  sessions).
- The retry ladder's throttle handling is core behavior, not host policy:
  retryable `Quota` failures carrying `Retry-After` defer without consuming
  attempts, bounded by the cumulative `ProviderRetryPolicy::
  throttle_wait_budget_ms` (default 90s, `0` disables) and eight courtesy
  calls. Total provider calls are bounded by eight plus `max_attempts`
  independently of `Retry-After`; after either courtesy bound is exhausted,
  throttles count as ordinary retryable failures.
- Nothing in core recognizes an admission decorator, so nothing in core can
  observe or persist one: hosts own the wrapper's metrics, and a provider
  rebuilt from a `ProviderSpec` comes back unwrapped.

## Considered Alternatives

- **An `AdmissionController` trait in core with a shipped AIMD default.**
  Rejected: the shipped default becomes de-facto policy lash would have to
  tune for every deployment shape, and the trait would only re-state what a
  decorator already does over the existing `Provider` contract. ADR-0014's
  division — host owns policy, lash owns levers — applies unchanged.
- **A host-assigned dispatch-class field on `LlmRequest`.** Rejected as
  host-to-host tunneling through core: the host would set the field solely so
  its own decorator could read it back, making core carry (and persist)
  vocabulary it never interprets. `scope.session_id` already gives the host a
  classing key it controls end to end — it is contractually present on every
  provider request, and even direct calls accept a host-chosen session id
  (`crates/lash-core/src/direct.rs`).
- **A general metadata bag on `LlmRequest`.** Rejected as a junk drawer:
  unversioned stringly side-channels between host layers, with every future
  "just one field" request landing there instead of being designed. Everything
  admission needs is derivable from session identity plus host state.
