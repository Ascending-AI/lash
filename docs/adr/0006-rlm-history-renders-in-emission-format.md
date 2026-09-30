# RLM history renders in the emission format

## Status

accepted

## Decision

In RLM's cell channel, history uses the grammar the model emits: one assistant message containing prose and a `<typescript>...</typescript>` cell, followed by a user observation containing stored printed text, error and final value. The dialect renderer folds buffered assistant prose into its trajectory entry. Standalone messages retain their roles and use bounded previews with `history[N].content` handles for full retrieval. ADR 0083 owns native-channel exchanges; ADR 0116 owns opaque tool results.

## Rules and guarantees

A committed assistant transcript is the canonical representation of a completed turn. The history projection uses chronological provenance to omit that turn's redundant successful terminal step, paired assistant content and unobserved echo. A terminal step without a committed assistant message stays visible. Content equality is not the criterion.

Printed values retain both their typed value and text rendered at cell completion. Prompt construction reuses stored text. Prompt messages and `RlmHistoryProjection` use compact canonical indices; omitted protocol entries consume no `history[N]` index. Output and attachment retrieval handles use those same indices.

The rolling cache breakpoint marks the last canonical history message before the current iteration's volatile tail. Failed cells remain visible while repair is needed; the prompt drops repaired failure branches within the same user/event turn boundary, identified by plugin provenance. Durable history remains append-only.

## Why and alternatives

Showing assistant history in a wrapper the model never emits encourages it to imitate that wrapper. An anti-echo instruction leaves that representation mismatch in place and is rejected. Splitting prose and code into two assistant messages is rejected because the emitted step is one message and provider role alternation needs a clean step/observation pair.

## Consequences

History rendering and emission share one cell grammar. Stored prints keep their original rendering across resume. [History rendering](../../crates/lash-protocol-rlm/src/driver/history.rs) and [semantic history projection](../../crates/lash-protocol-rlm/src/projection/context.rs) own the implementation.
