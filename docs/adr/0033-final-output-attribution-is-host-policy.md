# A turn has no single producing model; attribution is host policy

ADR 0031 proposed that lash compute "what produced the final output" as a tagged provenance
value on the turn result. That was over-reach, imported from single-run agent frameworks
whose model is "one run yields one final assistant message." Lash's model is not that shape,
and forcing a turn-level producing-model onto it manufactures an answer that is arbitrary in
exactly the interesting cases. We decided lash exposes execution facts at **call
granularity** — per-call `ExecutionEvidence` (served model, response id, request id,
reasoning tokens, finish reason) plus the per-attempt ledger, delivered as the per-call
`LlmCallRecord` list on `TurnReport` (ADR 0032). **Lash computes no turn-level or
session-level model attribution** — no "the served model," no "primary model," no
final-output provenance tag. Any higher-level view is composed by the host from the ledger
it already holds.

A turn is inherently a multi-model composite, in every protocol, not only under RLM:
subagents run on their own model tiers; a per-turn selection can differ from the session
model; a transport fallback (router-side routing, or a retry landing on a different served
model) means even one logical call can produce a model other than the one requested; and a
tool loop is many calls with no guarantee of one model. On top of that, the *visible*
output may map to **zero** model calls — an RLM `finish <value>` Final Value, or a tool
result promoted to output, ends a turn with no assistant-text call producing it. "The model
of a turn" is therefore a category error: the honest answer is often several models, or none.
A single-value rollup would be both lossy (it discards the multi-model reality) and
redundant (the calls are already in the ledger).

This is the same seam philosophy as ADR 0014 (operational policy stays with the host) and
ADR 0026 (model facts are host-supplied data): *which* call counts as "the" model for a
displayed message depends on what the host's surface means by a message — a product
question, not a framework contract. The host has everything it needs: each turn result
carries its per-call ledger, and a session is a sequence of those results, so any
turn- or session-level attribution the host wants is a projection over records it already
holds. A downstream chat UI that wants a per-message "model badge" derives it from the
ledger with its own turn semantics — and for a multi-call, possibly-Final-Value turn the
honest answer may be the resolved intent or the set of contributing models, not one
fabricated model. That decision lives with the host.

Consequences: the "tagged final-output provenance" clause of ADR 0031 is withdrawn, and the
runtime-side arc of ADR 0032 ends at ledger aggregation onto `TurnReport` plus its remote
mirror — there is no provenance-selector step. `TurnOutput`/`TurnReport` carry no producing-
model identity of their own (they never did); this ADR makes that a deliberate, permanent
boundary rather than a gap to be filled.
