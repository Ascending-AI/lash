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
2. **The realization invocation.** `LashToolRealization` is a pinned Restate
   service (one lane per build generation, like the process workflow). Its
   `realize` handler admits a controller over the Run's admitted execution
   scope, so intent identities, effect addresses and referrer claims are the
   ones the Run would have minted, and realizes the intents through the
   deployment's `ToolRealizer`. Every command realization issues lands in this
   invocation's journal. A replay of the realization replays only its own
   commands; a replay of the Run never meets them.
3. **Receipt.** The realization answers a `RealizationReceipt`: the ordered
   `ToolIntentExecutionOutcome` of every intent. The attach handle is a Run
   schedule choice beside the issued X results. Only a fresh schedule's
   selector awaits it, exactly as it awaits an X result half; a served
   schedule never polls it. The window that selects it records
   `Realized { call_id, receipt }`, with the receipt as Run-owned material.
   The Run adopts the recorded receipt (live and on replay alike) before V
   presents, and V settles the declarations as before. Realization therefore
   never polls an SDK result beside the scheduler.
4. **Declared starts.** A declared start's launch and its discharge make no
   journal commands: the launch registers under its stable `StartKey` and the
   registrar answers the process it registered first, and discharge follows the
   recorded decision through idempotent effects. They stay in the owner-driven
   preparation beside the schedule. They are not part of the realization
   invocation.

## Identities and idempotency

- The realization key is `run:{opener}:{call}:realize`: the Run's opener and
  the final's `ToolCallId`. A Run realizes each final at most once.
- The send carries the key as its Restate idempotency key, on the Run
  handler's own generation lane. A duplicate send under the key attaches to the
  first invocation.
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

An admitted realization is issued work. `quiesce` selects every admitted
realization's receipt, as it selects every issued X's durable acknowledgement,
before the cut is capturable. The transfer carries the `Realized` record and its
receipt material, so the successor adopts the receipt by reference and presents
without realizing again. The predecessor's realization ran once; the successor
receives its receipt once.

## Consequences

- `SingletonToolHandlers` does not realize intents. A handler answers the
  realization's payload for a final (`realization`) and adopts the receipt
  (`adopt_realization`). The deployment's `ToolRealizer` executes the payload.
- The protected preparation beside the schedule only launches and discharges
  declared starts.
- A final with intents costs one more schedule record (`Realized`) and two
  commands (send, attach) in the Run's journal, and one invocation.

## Rejected alternatives

- **Realize in a dedicated Run frame.** Serializing realization into the Run's
  journal keeps the commands at deterministic positions, but holds every other
  call's result behind the realization. The protected drain requires that a
  final's held declarations never stall unrelated effects.
- **Realize inside a process.** A process is admitted before its body runs and
  owns its journal, but it is a model-visible value with its own owner, lineage,
  wake and retention. Every intent would realize under the process's owner
  instead of the session's, changing identities and lineage.
- **Attach to an in-flight realization from the successor.** The successor could
  re-send under the same key and attach. A finished invocation's lane is
  deregistered once its build drains, so the successor's attach would race the
  predecessor's deployment removal. Selecting the receipt before capture leaves
  nothing in flight to transfer.

## Evidence

The laws are in `crates/lash-restate/src/tests/run_coordinator_on_the_double/realization.rs`.
