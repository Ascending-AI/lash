# Behavior transcripts are one normalized vocabulary

## Status

Accepted.

## Decision

Scenario harnesses render behavior transcripts through
`lash_core::testing::behavior_transcript`, also available as
`lash::testing::behavior_transcript`. The renderer owns a closed vocabulary,
fixed column widths, identifier aliases, free-text scrubbing and size formatting.
Harnesses extract facts from product state and pass them to the renderer.

```text
<actor>      <kind>    <event>                 [<key>=<value> ...]
```

| Family | Kinds | Meaning |
| --- | --- | --- |
| `boundary` | `ingress`, `park`, `resume`, `cancel`, `worker`, `fault` | A durable boundary or execution handover |
| `step` | `provider`, `tool`, `exec`, `spawn`, `await`, `wake`, `lease`, `observe` | An ordering event within a boundary |
| `durable-write` | `commit`, `effect` | An accepted durable write |
| `terminal` | `outcome` | An actor's terminal state |

A vocabulary change updates this decision in place. Event names within a kind
belong to the harness. A checkpoint commit includes typed usage and component
lines, distinguishing stored bodies from unchanged references.

## Why

A shared grammar lets a reviewer compare scenarios across harnesses. Separate
renderers make differences in formatting look like differences in behavior.

Line position carries order. Sequence numbers would renumber the tail after an
insertion, and content-derived column widths would change unrelated lines.
`Attr::id` assigns first-mention aliases within namespaces; text scrubbing
collapses whitespace, masks UUID and long-hex strings, and truncates text.
Harnesses can pin aliases they already own. The typed vocabulary has no clock
or duration attribute.

`Transcript::render` checks an 80-line review budget. A harness chooses a larger
budget explicitly with `with_review_budget`. A transcript supplements contract
assertions and property laws; it does not replace them. Mutation evidence for
expectations follows ADR 0044.

The renderer consumes strings, numbers, booleans and JSON values without
importing runtime state. A checkpoint observer records a commit only after the
backend accepts it. These boundaries keep the review artifact tied to observed
behavior rather than facts reconstructed by the renderer.

## Where it lives, and where it does not

The implementation lives in `lash-core-execution` and is re-exported by
`lash_core::testing` under the `testing` feature. The shared
`sansio_transcript` projection maps protocol effect streams to this vocabulary.
`lash-sansio` does not depend on this higher-level testing implementation.
Store conformance lives in `lash-internal-conformance`.

## Consequences

- Harnesses share line kinds, normalization and review budgets.
- `SimulationTrace::render_transcript` uses this renderer and pins its actor
  aliases. Actor columns and turn attributes use the same grammar for every run.
- Assertions describe the contract; transcripts expose scenario ordering and
  durable-write shape for review.

## Related decisions

ADR 0007 defines scenario-harness ownership. This decision defines their
behavior-transcript format.

## Code evidence

- [Vocabulary, attributes and rendering](../../crates/lash-core-execution/src/testing/behavior_transcript.rs#L79).
- [Core testing exports](../../crates/lash-core/src/testing/mod.rs#L26) and
  [facade export](../../crates/lash/src/testing.rs#L83).
- [Accepted checkpoint observation](../../crates/lash-core/src/testing/checkpoint_observer.rs#L369).

## Model usage data

Usage and attempt evidence ride the model call's recorded result
([ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md)). Behavioral transcripts
retain their normalized vocabulary; billing belongs to the host's provider decorator.
