# Context management uses views or frames

## Decision

The durable session graph is append-only. Context management either computes an ephemeral Prompt View over retained content or opens an Agent Frame seeded with continuation context. Core owns frame opening; plugins and hosts choose the cut, summarization prompt, summarizer and seed. `AgentFrameReason` is an open label.

## Rules and guarantees

Prompt View transforms and compactors hold read-only session views, effect controllers, direct completions and trace emitters. They cannot append nodes or open frames. A context-pressure hook returns `Continue`, `Record` or `OpenFrame`; core writes its decision. Hooks run in priority order before Prompt View transforms, once per physical turn, and the first frame opening ends the round. Hook identity includes both plugin and hook ids.

A pressure frame commits the outgoing records, frame, seed, artifact carries and execution-state reset together before the turn's model call. A later turn failure leaves that frame durable. Every accepted frame open clears stored execution state and prompt usage and restarts the live protocol from the seed. Initial-frame construction is the bootstrap exception. A staged open refuses a seed carrying artifacts because it cannot commit their transfer.

`compact_context` is a session command applied at a turn boundary under its sealed drive fence. It records its base head and frame under the run's compaction ordinal before summarization. Replay adopts that base and uses the same frame key and commit receipt. The frame commit settles the command. `continue_as` uses the same frame-opening invariant at final commit.

A host's durable frame open on a store-backed session is also a session command, `SessionCommand::OpenAgentFrame`, applied at a turn boundary against the boundary's resident head. The drive opens the frame with its seed in the commit that settles the command, under the command root's fence, and then restarts the live protocol from the seed. The command settles with the opened frame and the node ids the commit persisted, or with the open's typed refusal, such as a switch to a historical frame; a refused open writes only its settlement. Only a storeless runtime opens a host's frame directly.

Summarizer execution is at-least-once, with exactly one committed frame. Once a summary result is journaled, replay reuses it. A crash between the provider answer and publication of its journal record requests the summary again on redrive. Repeated crashes before publication can repeat the call without a fixed call-count cap. Replay after frame publication adopts the journaled compaction base and reuses the same frame key and commit receipt.

## Alternatives and consequences

Persistent history rewriting is rejected because it erases inspectable context and confuses durable history with the view of one model call. Pruning and image elision therefore operate on Prompt Views. Durable compaction opens frames, so history remains inspectable and retries can meet an idempotent frame receipt.

The implementation is in [context hook services](../../crates/lash-core-execution/src/plugin/history.rs), [pressure decisions](../../crates/lash-core/src/runtime/turn_loop/context_pressure.rs), [recorded compaction bases](../../crates/lash-core/src/runtime/compaction_base.rs) and [frame opening](../../crates/lash-core/src/runtime/frame_open.rs).
