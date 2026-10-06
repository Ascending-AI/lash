# Attempt history rides inside the result

## Context

One successful response does not describe the failed or interrupted transport
attempts that precede it. Hosts need the attempt history without a second
persistence protocol beside the effect result.

## Decision

The retry-owning provider boundary seals one immutable `AttemptRecord` per
transport invocation. `ProviderCompletion` and `ProviderCompletionError` both
carry the complete `LlmCallRecord`. The model call's committed phase record
carries that record with its outcome; there is no separate attempt log.

An attempt is a transport invocation, not a retry-budget unit or an admission
wait. A courtesy retry can produce another attempt without consuming retry
budget. Each record has an ordinal, `Completed`, `Failed`, `Aborted` or
`Interrupted` outcome, protocol position, budget-consumption fact and optional
retry decision. A scheduled retry owns its delay, throttle/backoff wait and
typed retry class; a declined retry owns its typed cause. Host charge-safety
evidence belongs to the corresponding class or cause. `Aborted` means explicit cancellation; `Interrupted` means
observation ends without a provider terminal or declared cancellation.

A Lash-minted `LlmCallId` identifies the logical call above retries. The pair
`(call_id, ordinal)` identifies an attempt. Provider request and response ids
remain evidence, not call identity. Records retain observed evidence and
usage with absence intact, plus structured errors. A normalized error records
a typed provider failure kind, HTTP status, transport request id, retry-after and an opaque
namespaced failure code. It does not persist the provider's diagnostic prose.
Raw diagnostic text that Lash exposes is live observation for host sinks.

`FailureCode` preserves the author's namespace. Trusted decoding of recorded state
uses `from_wire`; foreign input uses `from_foreign_wire` and cannot acquire a
reserved namespace by spelling one. Host and plugin namespaces are validated.
These are vocabulary ownership rules, not an authentication policy.

The turn report aggregates calls from that session. Child-session calls remain
on child results. Lash exposes no final-output provenance selector (ADR 0033).
Durable attempt history belongs to the execution that produces the recorded
outcome. A crash before that outcome commits can lose its attempt history;
the call is re-sent under [ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §4.

The full Prompt View remains authoritative on every call. A provider may reuse
a disposable cached response id only after validating the current request's
non-input fingerprint and cached input-plus-response prefix. Drift declines
reuse. Provider cache state is not durable session authority.

## Consequences

Resume loads the recorded ledger with the call's result without rebuilding
it from telemetry. A separate append-only attempt log is rejected because
it needs its own redelivery deduplication, retention and reconciliation with
the committed result. The accepted trade is that an unrecorded crash-era attempt is not
durable billing evidence. Hosts own any supplementary live telemetry archive.

## Implementation

- [Call and attempt types](../../crates/lash-sansio/src/llm/types.rs) and [retry ownership](../../crates/lash-core-llm/src/provider/handle.rs).
- [Runtime call result](../../crates/lash-core/src/runtime/turn_driver/local_effects.rs) and [turn report vocabulary](../../crates/lash-core-execution/src/runtime/vocabulary.rs).
- [Failure-code decoding](../../crates/lash-sansio/src/session_model/failure.rs).
- [Cached-prefix validation](../../crates/lash-provider-openai/src/codex/continuation.rs).

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.
