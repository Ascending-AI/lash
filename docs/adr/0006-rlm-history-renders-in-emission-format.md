# RLM History Renders in the Emission Format

## Status

accepted

Amended 2026-09-24 (FIG-3016, FIG-3021): [ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md)
makes TypeScript the sole RLM dialect, so the emitted cell is
`<typescript>…</typescript>` and the renderer is the tag-parameterized
`render_cell_text`. The decision is unchanged.

## Decision

The RLM history renderer presents each prior step in the exact grammar the model must emit. A prior executed step renders as an **assistant** message containing `{prose}\n<typescript>\n{code}\n</typescript>` — the canonical cell produced by `render_cell_text` — followed by a **user** message carrying that step's printed output, error, and final value. Plain user turns and prose-only finishes render their content verbatim by role. The `--- history[N] · … ---` header, the `Code:` framing, the indented source, and the inline `history[N].output[M]` gluing are removed entirely: **history format equals emission format**. The model's reasoning lane, the `history[N].output[M]` runtime binding, and the event store are unchanged.

## Why

RLM requires the model to emit a paired `<lashlang>…</lashlang>` cell (`cell_scan.rs`), but lash rendered prior steps back into the prompt in a different meta-format inside assistant-role messages (`driver/history.rs`). A model shown its own past turns in a wrapper it never emits imitates the wrapper. Observed live (`z-ai/glm-5.2`): the model produced `--- history[31] · assistant message · 0 chars ---` / `--- history[32] · lashlang step · protocol_iteration 2 ---` as its turn; the extractor found no cell; under `Natural` termination the turn finished with that echo as the user-facing answer.

The root cause is a representation mismatch — the input history format differs from the output emission format — compounded by placing the meta-format in assistant-role messages, which trains the model in-context to continue it. Rendering history in the emission grammar removes the mismatch by construction. This is the design every mature code-as-action agent converges on (smolagents replays the verbatim code in the assistant role; CodeAct/MINT and OpenHands do the same via tags or the native tool channel).

## Consequences

- **One renderer, one format.** The meta-format header strings, the `Code:` framing, the inline `history[N].output[M]` gluing, and the `indent_source` helper are deleted, not flagged. No `RlmHistoryFormat` enum, no env switch, no dual path. Helpers left with no caller (`message_role_label`, `indent_source`) are removed in the same change.
- **Step folding.** A step is stored as two consecutive chronological entries — an assistant prose `Message` then a `RlmTrajectoryEntry`. The renderer folds them into one assistant message (prose then cell). `lash_core::visit_turn_view` is a push visitor with no lookahead, so folding uses a pending-prose buffer flushed at end-of-iteration; a prose entry with no following step renders standalone (the prose-only finish case).
- **Completed-turn precedence.** A committed assistant transcript message is the canonical representation of a completed turn. When the chronological event order proves that a successful terminal step and a later assistant message occur before the next user/event turn boundary, the terminal step, its paired `RlmAssistantContent`, and its never-observed output echo are omitted. This is provenance-based precedence: message and finish content are never compared. A terminal step with no same-turn committed assistant message remains in trajectory form without information loss.
- **Caching preserved.** History stays append-only and the rolling cache breakpoint (`mark_last_history_text_cache_breakpoint`) still fences the last canonical history message before the volatile current-iteration tail. Existing sessions incur one stable-prefix change when this rule rolls out; subsequent turns extend the new canonical prefix normally.
- **Canonical projection indices.** The prompt renderer and `RlmHistoryProjection` run the same completed-turn canonicalization. `history[N]` is a compact semantic list: suppressed protocol-internal entries consume no index, and rendered `history[N].output[M]` / attachment re-fetch handles remap chronological source indices to the resulting compact index.
- **No migration.** History is a pure function of stored `SessionEventRecord`s computed at prompt-build time; a resumed session renders in the new format on its next turn. No schema change, no backfill, no dual-read window.

## Considered Alternatives

- **Keep the meta-format, add an anti-echo system instruction.** Rejected: it patches the representation mismatch with prose instead of removing it; weaker models still imitate the salient in-context format.
- **Native tool-call transport (cell as `tool_use`, result as `tool_result`).** Rejected: it pushes lashlang source into a JSON-string argument — the encoding code-as-action exists to avoid — and contradicts RLM's deliberate empty tool array (`tools: Arc::new(Vec::new())`, `tool_choice: LlmToolChoice::None`). It buys no caching or robustness the in-format text rendering lacks.
- **Two assistant messages (prose, then cell) instead of folding.** Rejected: back-to-back assistant turns do not match how the model emits and stress provider role-alternation; folding yields clean `User → Assistant → User` alternation, one step per pair.
