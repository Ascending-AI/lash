# Turn cancellation is a first-party work-driver primitive on the keyed-promise seam

Foreground turns need a durable, externally addressable stop request without becoming Runtime
Processes and without adding coordination state to the session store. We therefore define exact
turn cancellation as `TurnAddress { session_id, turn_id }` on `TurnWorkDriver`, alongside (but
separate from) `ProcessWorkDriver`. Session and turn ids are routing identities, not authorization
credentials; every host boundary remains responsible for authentication and authorization.

The primitive is cooperative. A request races the turn's normal completion through a reserved,
first-writer-wins keyed promise. A cancellation winner carries a request id, optional opaque
host-supplied origin, and optional reason as evidence; the running or replayed owner feeds that
evidence into its internal cancellation token, assembles `TurnStop::Cancelled { evidence }`, and commits under
the live session-execution lease. A normal completion seals the same gate before commit, causing
later requests to report `CompletionWonRace`. A second reserved promise publishes terminal
evidence after the commit so an external caller can attach without polling storage. The promise
key uses semantic session/turn identity rather than lease generation: cancellation survives owner
loss. The final commit remains governed by the session-head CAS and any claim-ownership checks
(ADR 0029), after the owner reads and settles the pre-commit cancellation gate;
lease loss alone does not reject a current-head commit.

The cancellation receipt reports only the outcome of addressing the keyed promise. Persistent
Native sessions delegate the three reserved turn-control aliases to the session store's SQL
promise coordinator; Native effects and all other waits remain process-local. Reopening the same
SQLite catalog or PostgreSQL database therefore recovers the same cancellation keys. A configured
durable effect host such as Restate retains ownership of its own turn-control authority and its
journaled observations.

Closing those promises is a crash-completable protocol. Before resolving either gate, the current
session-execution holder persists one exact, non-overwritable closure authorization for the turn.
It records the chosen control binding, admitted physical execution scope, the three reserved keys,
the proposed base terminal, the observed cancellation-intent revision, and the authorizing lease
generation. A vacant slot accepts the operation, an identical retry adopts it, and a different
operation conflicts. Lease renewal, release, and takeover preserve the slot. A successor may finish
the same idempotent promise resolutions, including adopting a legitimate different first writer,
but only a current lease fence may apply the resulting input disposition, commit the turn, publish
terminal evidence, or consume the slot. Promise settlement and the store mutation are deliberately
separate authority domains; the durable authorization bridges a crash between them without becoming
a second winner record.

Every store-backed activation validates the selected binding and drains pending closure operations
before accepting input, draining commands, invoking a model, or doing follow-on work. The drain
settles the exact reserved gates, derives disposition from their authenticated winner, and consumes
the authorization atomically with input repair or final commit. An intent CAS refusal retains the
authorization and retries only the refreshed predicate; it never reruns model calls, hooks, effect
construction, or usage staging. Unknown or revoked promise evidence is a typed failure and leaves the
authorization pinned. A missing cancellation intent does not justify sealing a future gate: repair
rechecks absence transactionally and performs only the ordinary input deferral.

Who cancelled is host-domain data: Lash records an opaque host-supplied origin and never interprets
it, mirroring ADR 0026's treatment of host-supplied capability data. Process-local token entry
points synthesize `internal:<turn_id>` request evidence because they do not traverse the addressed
request gate. A host with a known origin can supply its own vocabulary, such as `"user"` or
`"shutdown"`; a raw `TurnBuilder::cancel(CancellationToken)` honestly records no origin.

Turn cancellation has three operational layers:

1. `TurnWorkDriver::request_cancel` is the cooperative foreground-turn primitive. Its inline tier
   is process-local; its durable tier survives owner-process loss and replay. It can unwind
   cancellable provider/tool waits, but it cannot guarantee that detached tasks, subprocesses, or
   non-cooperative providers have stopped.
2. Runtime Process cancellation remains the existing process event and worker-recovery protocol.
   Foreground turns do not acquire Process identity, ownership, or lifecycle (ADR 0003).
3. Engine invocation cancellation or kill is host-owned break-glass recovery. Per ADR 0019, owner
   destruction is not cooperative evidence and must never be projected as Lash `Cancelled`; the
   authoritative result is unknown unless a live/replayed owner commits one.

For turns blocked in local composite execution, graceful cancellation cannot interrupt that work;
the demonstrated break-glass is an admin `KILL`, run last because a killed handler cannot release
the shared-session lease.

The keyed-promise implementation uses the existing `AwaitEventResolver` operations and the
configured cancellation authority. Reserved `TurnCancelGate` and
`TurnTerminal` identities are indexed as control promises: ordinary durable-wait
cancellation does not sweep them, while session deletion revokes them. This adds
no second replay journal (ADR 0012) and no claim
TTL. The gate is the only stop signal: nothing waits on, polls, or coordinates
through the store to learn that a turn was cancelled, so the wait stays on the
work-driver seam (ADR 0016). The shared SQL coordinator may poll its own
authoritative promise row after a missed notification; intent and projection
rows are never polled as a stop signal. A live owner does hold an
engine-native keyed-promise observation; Restate implements that observation
through `LashDurableWaitWorkflow` ingress with bounded retry, not its Admin API.
The inline registry drains live gate/terminal entries after terminal publication
and keeps only bounded recent completion and session-revocation caches.

Vacuum does not remove pending closure authorizations. Session deletion and Process-scope retirement
inspect the durable session-to-scope pins first and refuse destructive cleanup while any matching
operation remains. First-party memory, SQLite, and PostgreSQL factories expose that inspection at the
lifecycle boundary; custom factories must implement it and fail closed when they cannot. Normal
activation owns the drain. Administrative cleanup cannot erase an authorization merely because its
original lease owner disappeared.

We rejected the store as a *coordination* mechanism for cancellation — a lease
marker, or a row that a waiter polls — because that adds store coordination,
polling, and recovery races. A durable turn-cancel request row does exist
(`record_turn_cancel_request` / `turn_cancel_request`), and it is load-bearing
as an intent and receipt projection. The keyed gate pair alone decides whether
cancellation won and which base or escalated evidence the turn honours. Only
that settled evidence may select the policy for active-turn input the cancelled
turn never delivered. Teardown and orphan repair read durable intent so they
know which unresolved gate to reconcile, then project the gate's effective
winner back into the row. The row is not a stop signal, arbitration result, or
channel any waiter observes. We
rejected invocation-id cancellation because it leaks engine identity and can
destroy an owner without a Lash result; turns-as-processes because ADR 0003 keeps foreground turns
session-owned; and session-wide cancel-all because it needs an active-turn index and can touch the
wrong or a future turn. A host that offers “stop all visible work” retains the exact active turn
ids it submitted and fans out exact requests.

## Cancel modes: immediate abort and after-step stop (FIG-635)

A request carries a host-chosen `TurnCancelMode`. `Immediate` is the abort
described above: the owner feeds the evidence into its cooperative token as
soon as it observes the gate, in-flight provider and tool waits unwind, and the
uncommitted tail backtracks to the last checkpoint (FIG-408). On a
controller-owned journal, Immediate lands between journal commands: the start
gate, the after-LLM gate, and the after-step gate are the journaled
observation points, so a replay takes the same command path as the original
attempt.

`AfterStep` is the stop that loses no work. The owner defers the request until
the step boundary that closes the current protocol iteration: the response
has streamed, every tool call of that iteration has completed, and the
iteration's checkpoint has committed. It is observed there under the
replay-deterministic identity `turn_cancel.after_step.{iteration}` on every
binding, after the commit, and honoured by finishing the turn with
`TurnStop::Cancelled` whose evidence names the mode and the iteration. The
cooperative token never fires for an after-step request, so tools run to
completion and never see a cancelled token, and nothing backtracks. A turn
that has not started yet is refused at the start gate in both modes. An
after-step request that lands during a durable sleep composes with
cancel-at-wake (FIG-2321): the wait completes, the iteration finishes, and the
stop honours at its boundary. The undelivered-input disposition applies in
both modes; a stop never drains queued work.

The gate itself stays first-writer-wins, so a stronger request cannot rewrite
it. Its accepted request permanently owns the undelivered-input policy.
Escalation rides a third reserved promise, `TurnCancelEscalation`, written only
by an `Immediate` request with the same policy that found the gate holding an
`AfterStep` request; escalation changes timing while the durable base projection
retains the original policy acceptor. A different policy reports
`PolicyConflict { requested, accepted }` before touching escalation. A
same-or-weaker request still reports `AlreadyRequested`. Lash ships no
escalation timer; "abort if the step has not finished after N seconds" is host
policy expressed as a second request.

Restate durable waits carry the gate payload. The wake an awakeable
journals is derived from the gate resolution that settled it, so an
`Immediate` request unwinds a parked sleep, await-event or process await at
that wake exactly as before, while an `AfterStep` request lets the wait
finish on its own terms: the iteration completes and the turn stops at its
step boundary. A deferred wait re-parks on the turn's escalation promise, so
a later `Immediate` request still unwinds it mid-wait.

## Terminal product-event ownership

The turn execution publisher owns the observer-facing terminal event. A Stop
handler attaches to terminal evidence and returns its receipt; it does not
publish another terminal event, including for repeated requests or a completion
that won the race. Removing a dangling route is not evidence of a failed turn.
Cancellation traces use the request id in the recorded cancellation evidence,
so a losing request attributes the stop to the same winner as the terminal.
