# Context Management Uses Views Or Frames

Lash durable session graphs are append-only: context management is either an ephemeral Prompt View over existing durable content or a durable Agent Frame transition that seeds new continuation context. Persistent history rewrites are deliberately rejected because they erase inspectable prior context, blur the boundary between "what happened" and "what this model call sees", and make storage/replay semantics harder to reason about.

Consequences: an administrative compaction (`compact_context`) opens a compaction Agent Frame rather than rewriting committed history; standard-compaction style pruning and image elision are Prompt View transforms only. Prompt View transforms may use effects, including LLM calls, but their output is ephemeral and they must not mutate durable session history or open durable Agent Frames. `AgentFrameReason` is an open label so plugins and hosts can name frame transitions without core branching on every reason.

Core owns the generic frame-opening primitive and the append-only invariant. Compaction strategy belongs to plugins or hosts: they choose the cut point, summarization prompt, child summarization turn, and summary seed message. The runtime commits the resulting seed nodes by opening a frame with the compaction reason.

No public or plugin API should expose a durable history-rewrite entry point after this cutover. Compaction-facing APIs must be frame-oriented so callers cannot accidentally reintroduce persistent history rewriting.

## Amendment (FIG-4110): pressure compaction is a hook, not a transform

Prompt View transforms and compactors hold read-only services: a session read view, the effect controller, direct completions and trace emission. Neither can append nodes or open a frame.

Core calls each plugin's context-pressure hook once per physical turn, before the Prompt View transforms, with the previous provider-reported prompt usage, the context window and the committed read view. The hook may run effects. It returns a decision: continue; record plugin nodes; or open a compaction frame with a task and seed nodes, after recording nodes in the frame it leaves. Hooks run in priority order, and the first frame opening ends the round.

Core writes the decision. Records join the turn's commit. A frame opens through the one frame-open primitive and commits on its own, before the turn's model call, exactly as an administrative compaction's frame does: the records in the frame being left, the frame, its seed and the execution-state reset become durable together. The turn then runs in the new frame, and a later failure of the turn leaves the frame in place, so a retry runs in it without summarizing again. Core derives the frame key from the session, the frame current at open, the turn and the hook; the commit is a fenced, idempotent store write a redrive meets by its receipt. A turn that starts after a pressure frame and ends in `continue_as` commits its own frame after the pressure frame, in order.

Every frame open, whoever authors it, clears the stored execution state and the prompt usage the previous frame measured, and the live protocol session restarts from the new frame's seed. The threshold, the cut, the summarizer prompt and overflow recovery stay plugin strategy.

## Amendment (FIG-4133): `compact_context` replays its recorded base

Before its summarizer runs, `compact_context` records the base it compacts as one recorded step: the durable head and the frame current at its start. The step is keyed by the compaction's ordinal in its run. A redrive replays that base and adopts it, even when the compaction's own commit has already moved the head. It then reads the summary back from its journal over the same history, derives the same frame key from the recorded frame, and commits under an operation the frame key names, so it meets the first commit's receipt. It never opens a second frame from the moved head. A repeated `compact_context`, even under the same scope, is the run's next compaction: it records its own base and opens its own frame.

## Amendment (FIG-4134): every frame open is fenced, carries its seed's artifacts and resets live state

The frame invariants hold for every author: a context-pressure hook, overflow recovery, `compact_context` and `continue_as`.

- **One primitive.** Every open goes through the runtime's one frame-open primitive, which reaches the store's `open_agent_frame_in_state_with_clock`. The only exception is a session's initial frame, which is written before any frame exists to leave or any execution state exists to reset.
- **Carries.** An open derives what its seed carries out of the frame it leaves through one carry derivation: the artifacts the session's code executor finds in the seed. A protocol-specific seed therefore keeps its artifacts alive in the successor frame, whoever wrote the seed, and the commit hands them over as it ends the frame being left. An open that only stages its frame, without committing it, cannot hand artifacts over, so it refuses a seed that carries any.
- **Live reset.** Every accepted open restarts the live interpreter from the new frame's seed: after its commit when it commits, with a store or without one, and at once when it only stages.
- **Fence.** `compact_context` writes beside the drive. It records the drive fence current when it starts with its base, and its frame commit presents that fence, as the turn's and the pressure frame's commits present theirs; a replay presents the recorded fence, never a newer one. An admission sealed before the commit refuses it typed, with nothing of it durable, and the admitted turn proceeds. A redrive whose first execution already committed the frame reports that frame opened.
- **Hook identity.** A context-pressure hook is named by the plugin that registered it and its own id. Two plugins whose hooks share an id never share a record or frame namespace.
- **Replay (F3).** Once a summary's result is journaled, it is never requested again, and a crash anywhere yields exactly one frame. The provider call itself is at-least-once, like every model call lash makes. A crash between the provider's answer and its journal record requests the summary again on redrive: the summarizer is called at most twice, and the session still ends with exactly one frame.
