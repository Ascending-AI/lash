# Context Management Uses Views Or Frames

Lash durable session graphs are append-only: context management is either an ephemeral Prompt View over existing durable content or a durable Agent Frame transition that seeds new continuation context. Persistent history rewrites are deliberately rejected because they erase inspectable prior context, blur the boundary between "what happened" and "what this model call sees", and make storage/replay semantics harder to reason about.

Consequences: `/compact` opens a compaction Agent Frame rather than rewriting committed history; standard-compaction style pruning and image elision are Prompt View transforms only. Prompt View transforms may use effects, including LLM calls, but their output is ephemeral and they must not mutate durable session history or open durable Agent Frames. `AgentFrameReason` is an open label so plugins and hosts can name frame transitions without core branching on every reason.

Core owns the generic frame-opening primitive and the append-only invariant. Compaction strategy belongs to plugins or hosts: they choose the cut point, summarization prompt, child summarization turn, and summary seed message. The runtime commits the resulting seed nodes by opening a frame with the compaction reason.

No public or plugin API should expose a durable history-rewrite entry point after this cutover. Compaction-facing APIs must be frame-oriented so callers cannot accidentally reintroduce persistent history rewriting.
