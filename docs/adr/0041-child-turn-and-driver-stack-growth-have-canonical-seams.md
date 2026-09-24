# Child-turn and driver stack growth have canonical seams

Amended 2026-09-24 (FIG-3669), **not yet implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour: session-execution leases and ingress claims
owned by the child runtime. Those passages stay as written until the PR that
deletes the code (FIG-3667, FIG-3668, or FIG-3600 for the session lease)
rewrites them.

Nested managed sessions used to poll a child turn directly from the tool call
that created it. Each level therefore re-entered the complete logical-turn
future on its parent's poll stack. The resulting failure mode was poll-stack
depth: ordinary one-level child turns already needed oversized test-thread
stacks, even though boxing showed that no individual future was exceptionally
large.

We designate owned task boundaries as the intentional stack-growth seams. A
managed child turn with an owned, shareable effect controller runs in its own
`tokio::spawn` task. Inside a physical turn, heavy local effects (`LlmCall`,
`ToolInvocation`, `ToolAttempt`, `ExecCode`, and direct LLM calls) also run through
the runtime task seam. Every recursively re-entrant effect therefore starts
with a fresh task stack instead of extending the turn driver's poll stack.
Cheap leaf effects remain inline. Callers must not add opportunistic boxes
around whichever future most recently exceeded a stack budget.

The child task retains the managed runtime mutex for the whole turn and calls
`publish_from` while the post-turn state is still guarded. The mutex is the
child runtime's single-writer boundary, not a recursion guard, so shortening
its scope would change observation and mutation ordering. The existing event
channel remains the only child-to-parent observation path and preserves FIFO
delivery to the parent's session and activity sinks.

Cancellation uses an abort-on-drop guard around the child task. If the parent
select or turn is cancelled, dropping its child-turn future aborts the task
promptly; normal completion disarms the guard. A child task panic is resumed on
the awaiting parent so it keeps the former panic behavior instead of becoming
a silent or reclassified `JoinError`. Session execution leases, queued-work
claims, and turn-input claims remain owned and settled by the child runtime
under the same guarded turn path.

Heavy turn effects receive an owned driver snapshot containing cloned/`Arc`
runtime services, a cloned session view, an owned cancellation token, and an
owned controller proxy. Effect-local state changes that belong to the turn
driver (provider binding, stream summaries, and newly claimed checkpoint work)
are returned through a small update slot after the task joins.

Handler-scoped effect controllers remain on the engine invocation task. An
owned proxy sends controller operations back to that task, which drives them
from an explicit heap-owned LIFO stack. This preserves the former depth-first
journal order without recursively polling heavy effect futures. Local
executors stay on the spawned effect task: the proxy forwards the controller's
request to run local work and returns its result. Replay may therefore skip
local execution exactly as before, and Restate retains the original
handler-scoped journal controller rather than replacing it with an out-of-band
controller.

The preparation and plugin-abort preamble also lives in separate async helper
frames. Its `SessionReadView` and abort-only locals are destroyed before the
normal driver path clones state into `TurnBoundary`, avoiding transient graph
double-holds. Cancellation reconstructs its message sequence from the turn
boundary only after cancellation wins, and lease-loss cleanup borrows the
original claim vectors instead of keeping error-only clones live through final
commit.
