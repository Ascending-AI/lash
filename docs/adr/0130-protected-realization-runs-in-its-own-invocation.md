# ADR 0130: Protected realization runs in its own invocation

Status: Accepted

## Problem

A committed final's declared intents are protected work: once its decision is
durable and every lower rank is seated, its intents are realized behind their
exactly-once fences before its presentation (V). Realization issues journaled
commands of its own: every intent crosses `execute_effect`, and a trigger
emission issues several.

Restate's shared core replays a journal by position. It pre-enqueues every
recorded notification and refuses an await on an unresolved owned command
while later commands remain to replay (`UncompletedDoProgressDuringReplay`,
SDK 570). A Run's handler journal interleaves its schedule records with
whatever realization issued in the same windows. So a realization command in
flight at a crash, with a later X's decision recorded after it, cannot replay;
neither can a realization whose commands span schedule windows another call
wins. No placement inside the Run's journal fixes this without holding every
other call's results behind the realization, which the protected drain forbids.

## Decision

Intent realization runs as its own durable invocation, with its own journal.
The Run's journal records only the realization's admission and, later, the
receipt the realization answers.

1. **Admission.** A final whose capture declares intents admits its
   realization in its `declare` record: `DeclarationsIssued` and
   `RealizationAdmitted { call_id, key }` are one record, issued only when the
   drain frontier is open for its rank. Right after that record, in the same
   frame, the Run sends one request to the realization service and attaches to
   the invocation the send created. Both commands sit at deterministic
   positions of the Run's program, never beside the schedule.
2. **The realization invocation.** `LashToolRealization` is a journal-bearing
   Restate service, bound under its stable name and this build's generation
   name like the process workflow. The Run sends to the stable name, as a
   process's first segment does; Restate keeps the invocation on the
   deployment that began it. Its
   `realize` handler admits a controller over the Run's admitted execution
   scope, so intent identities, effect addresses and referrer claims are the
   ones the Run would have minted, and realizes the intents through the
   deployment's `ToolRealizer`. Every command realization issues lands in this
   invocation's journal. A replay of the realization replays only its own
   commands; a replay of the Run never meets them.
3. **Receipt.** The realization answers a `RealizationReceipt`: the ordered
   `ToolIntentExecutionOutcome` of every intent. The attach handle is a VM
   notification like an X result half, and it joins the Run's single
   first-completed selection over every selectable source: pending X results,
   timers and realization receipts. That selection is one combinator await,
   issued before the decision step it feeds, so a replay resolves it from the
   journal in recorded order; a served decision stays authoritative. The step
   that selects a receipt records `Realized { call_id, receipt }`, with the
   receipt as Run-owned material. The Run adopts the recorded receipt (live and
   on replay alike) before V presents, and V settles the declarations as
   before. Realization therefore never polls an SDK result beside the
   scheduler.
4. **Declared starts.** A declared start's launch and discharge are not part of
   the realization invocation. They run as the Run's own started step, issued
   once in the `declare` frame and selected like an X result, so no
   preparation runs beside the schedule.

## Identities and idempotency

- The realization key is `run:{opener}:{call}:realize`: the Run's opener and
  the final's `ToolCallId`. A Run realizes each final at most once.
- The send carries the key as its Restate idempotency key. A duplicate send
  under the key reaches the first invocation.
- Intent identities are unchanged: each intent's replay key derives from the
  session, the Run's execution scope, the call id, the intent index and the
  attempt invocation, exactly as before. The stores' exactly-once fences key on
  those replay keys, so a realization's own crash redelivery is fenced the same
  way an in-Run realization was.

## Cancellation

The admission follows the protected-final rule. A Run cancellation accepted
before the final's decision is durable withholds the final: it issues no
`declare`, admits no realization and sends nothing, so no external outcome
exists. A cancellation accepted after the decision cannot stop the realization:
the final still drains. The realization is a send, never a call, so Restate's
cancellation of the Run invocation does not propagate to it.

## Handover

An admitted realization is independently owned work. The Run records
`RealizationIssued { call_id, invocation_id }` after the idempotent send.
A physical cut waits for local X acknowledgements and transfers that record
and its `RealizationKey` alongside the rest of the Run (ruling #55). The
predecessor detaches its invocation-local selectable. It does not wait for an independent realization.
The successor attaches to exactly that invocation id, selects its receipt once,
and records `Realized` before V. It never sends another request or executes the
realization body. If the predecessor already selected the receipt, its material
transfers with the Run and the successor adopts it without attaching again.
An expired or missing engine result refuses the attach; it never authorizes a
fresh realization. No native future or VM notification key crosses the cut.

## Consequences

- `SingletonToolHandlers` does not realize intents. A handler answers the
  realization's payload for a final (`realization`) and adopts the receipt
  (`adopt_realization`). The deployment's `ToolRealizer` executes the payload.
- A final with intents adds an issued-reference record and a receipt schedule
  record (`Realized`), a send and an attach in the Run's journal, and one
  independent invocation.

## Rejected alternatives

- **Realize in a dedicated Run frame.** Serializing realization into the Run's
  journal keeps the commands at deterministic positions, but holds every other
  call's result behind the realization. The protected drain requires that a
  final's held declarations never stall unrelated effects.
- **Realize inside a process.** A process is admitted before its body runs and
  owns its journal, but it is a model-visible value with its own owner, lineage,
  wake and retention. Every intent would realize under the process's owner
  instead of the session's, changing identities and lineage.
## Integration with declared preparation

The landed start:prepare cut (FIG-5009) removes P and its receipt hand-off.
The drain selects start preparation and the child realization receipt as
independent outcomes. It issues V directly at the frontier after preparation
is acknowledged and Realized has been accepted and adopted. No receipt signal
or presentation future is raced beside the schedule. The pinned key/value
receipt remains intact for FIG-4998's single combinator and value demand gate.

## Evidence

The laws are in `crates/lash-restate/src/tests/run_coordinator_on_the_double/realization.rs`.
