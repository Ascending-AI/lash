# Context Management Uses Views Or Frames

Lash durable session graphs are append-only: context management is either an ephemeral Prompt View over existing durable content or a durable Agent Frame transition that seeds new continuation context. Persistent history rewrites are deliberately rejected because they erase inspectable prior context, blur the boundary between "what happened" and "what this model call sees", and make storage/replay semantics harder to reason about.

Consequences: `/compact` opens a compaction Agent Frame rather than rewriting committed history; standard-compaction style pruning and image elision are Prompt View transforms only. Prompt View transforms may use effects, including LLM calls, but their output is ephemeral and they must not mutate durable session history or open durable Agent Frames. `AgentFrameReason` is an open label so plugins and hosts can name frame transitions without core branching on every reason.

Core owns the generic frame-opening primitive and the append-only invariant. Compaction strategy belongs to plugins or hosts: they choose the cut point, summarization prompt, child summarization turn, and summary seed message. The runtime commits the resulting seed nodes by opening a frame with the compaction reason.

No public or plugin API should expose a durable history-rewrite entry point after this cutover. Compaction-facing APIs must be frame-oriented so callers cannot accidentally reintroduce persistent history rewriting.

## Amendment (FIG-4110): pressure compaction is a hook, not a transform

Prompt View transforms and compactors hold read-only services: a session read view, the effect controller, direct completions and trace emission. Neither can append nodes or open a frame.

Core calls each plugin's context-pressure hook once per physical turn, before the Prompt View transforms, with the previous provider-reported prompt usage, the context window and the committed read view. The hook may run effects. It returns a decision: continue; record plugin nodes; or open a compaction frame with a task and seed nodes, after recording nodes in the frame it leaves. Hooks run in priority order, and the first frame opening ends the round.

Core writes the decision. Records join the turn's commit. A frame opens through the one frame-open primitive and commits on its own, before the turn's model call, exactly as `/compact`'s frame does: the records in the frame being left, the frame, its seed and the execution-state reset become durable together. The turn then runs in the new frame, and a later failure of the turn leaves the frame in place, so a retry runs in it without summarizing again. Core derives the frame key from the session, the frame current at open, the turn and the hook; the commit is a fenced, idempotent store write a redrive meets by its receipt. A turn that starts after a pressure frame and ends in `continue_as` commits its own frame after the pressure frame, in order.

Every frame open, whoever authors it, clears the stored execution state and the prompt usage the previous frame measured, and the live protocol session restarts from the new frame's seed. The threshold, the cut, the summarizer prompt and overflow recovery stay plugin strategy.

## Amendment (FIG-4133): `/compact` replays its recorded base

Before its summarizer runs, `/compact` records the base it compacts as one recorded step: the durable head and the frame current at its start. The step is keyed by the compaction's ordinal in its run. A redrive replays that base and adopts it, even when the compaction's own commit has already moved the head. It then reads the summary back from its journal over the same history, derives the same frame key from the recorded frame, and commits under an operation the frame key names, so it meets the first commit's receipt. It never opens a second frame from the moved head. A repeated `/compact`, even under the same scope, is the run's next compaction: it records its own base and opens its own frame.
