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

The cancellation receipt reports only the outcome of addressing the keyed promise. Delivery
geometry is a property of the `TurnWorkDriver` and effect controller the Host Application
configured. The inline path uses a bounded process-global in-memory registry and is same-process
control only: a driver in another OS process can resolve its own local gate but cannot signal the
owner's gate. Cross-process cancellation and replay observation require a controller-backed
deployment such as Restate. A host decides whether a Stop control can honestly promise
cross-process delivery from the deployment it constructed, not from a receipt field.

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
configured effect controller's journal. Reserved `TurnCancelGate` and
`TurnTerminal` identities are indexed as control promises: ordinary durable-wait
cancellation does not sweep them, while session deletion revokes them. This adds
no session-store method (ADR 0016), no second replay journal (ADR 0012), no
store polling/watch path, and no claim TTL. A live owner does hold an
engine-native keyed-promise observation; Restate implements that observation
through `LashDurableWaitWorkflow` ingress with bounded retry, not its Admin API.
The inline registry drains live gate/terminal entries after terminal publication
and keeps only bounded recent completion and session-revocation caches.

We rejected store-side cancellation rows or a lease marker because they add store coordination,
polling, and recovery races; invocation-id cancellation because it leaks engine identity and can
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
it. Escalation rides a third reserved promise, `TurnCancelEscalation`,
written only by an `Immediate` request that found the gate holding an
`AfterStep` request; the durable record upgrades to the stronger request and
the receipt reports `Escalated`. A same-or-weaker request still reports
`AlreadyRequested`. Lash ships no escalation timer; "abort if the step has
not finished after N seconds" is host policy expressed as a second request.

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
