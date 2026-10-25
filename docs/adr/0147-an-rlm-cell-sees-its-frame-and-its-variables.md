# 0147: An RLM cell sees its frame and its variables

## Status

Accepted. `an_rlm_request_carries_no_history_surface_on_sqlite` in
`crates/lash-facade-tests/tests/facade_session/tests/output_retention.rs`
pins it.

## Context

An RLM cell's model saw three views of the past: the current frame's turns
rendered as messages, a `rlm.history` Read surface over the whole run's
transcript, and bound-variable rows labelled with the cell that last bound
each name. The second and third views cost a transcript index per send,
rebuilt from every committed record, and positional addresses
(`(await rlm.history.at(N)).output[M]`) in every observation. Over 1,000
unbroken turns that work grew the parent's CPU per send roughly threefold.

## Decision

1. A cell sees the current frame's turns and the session's variables, and
   nothing else. There is no transcript read, no transcript prompt section,
   no item schema and no producer label on a variable.
2. Context is bounded by the frame. `control.continue_as` opens a fresh frame
   from a `task` and `seed` variables (ADR 0112 §10); RLM has no compaction.
3. Printed output is display. A value a later cell needs lives in a
   variable. A cut print, message or retained value states what was shown
   and its total size, and names no address.
4. A function's `frozen_at` capture is kept: it names the snapshot the
   function reads, which is a different fact from who last assigned a name.
5. Retention out of history still bounds committed records. Its attachment
   is for hosts; no cell reads it back.

## Consequences

The late prompt block is the iteration, the bound variables and
finalization. Rendering the frame's turns stays, including the repaired
failure scrub and completed-turn precedence (ADR 0006). Durable cell
records, seed carry and the bound-variables listing stay.

## Why and alternatives

An incrementally maintained transcript index, or a store-indexed transcript
read, would keep recall for the model at the price of a second history
contract beside the frame. The frame and `continue_as` already bound what a
model needs; a value worth keeping is kept in a variable, where the model
chose to keep it.
