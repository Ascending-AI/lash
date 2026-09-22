# E2E Scenario: Context-Overflow Recovery

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just context-overflow-recovery-e2e` companion. Do not replace the
> companion's assertions with hand-written outcomes, and do not treat a green script as the
> judgment itself.

**Purpose.** Prove FIG-1272 end to end: a context-window overflow that lands *mid-turn* is
its own turn outcome, a host can read it off the public turn report, act on the compaction
seam lash already ships, and continue the same session — instead of being told only that
"the turn failed" and restarting.

The gap this closes is narrow and worth stating precisely. Lash already classifies an
overflow correctly, and its classifier is stricter than the implementations we compared
against. What it used to do with that classification was throw it away: the terminal reason
collapsed into `TurnStop::ProviderError`, the same stop an auth failure or a 500 produces.
The proactive compaction check cannot cover the case either, because it reads the
*previous* turn's reported usage and a tool result that arrives oversized inside the current
turn is invisible to it.

**Lash chooses no recovery policy.** Whether and when to compact, summarize, or restart
stays host policy. The kernel's whole obligation is to stop hiding the reason. This
fixture's policy — compact once, keep a one-line summary, run the next turn — belongs to
the fixture.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_CONTEXT_OVERFLOW_ARTIFACT_DIR=<fresh-dir> just context-overflow-recovery-e2e
```

It runs the scenario's single TypeScript row and writes it to
`<artifact-dir>/context-overflow-recovery/typescript/`. **Do not set
`LASH_RUNBOOK_DIALECT`.** This companion does read it — it uses the value as the artifact
*directory name* — but the binary hardcodes both the cell delimiters and the checkpoint's
`"dialect"` field to `typescript`. Setting it to anything else produces a directory named for
a dialect over evidence that says `typescript`, i.e. it silently mislabels the artifacts
rather than reproducing anything. ADR 0096 leaves one dialect; there is no second row to
reproduce. It emits
`context-overflow-recovery e2e passed: rows=N` only after the focused contract test and
every row's gates pass.

**No container, no token, no network — but not no build.** The store is a SQLite scratch
directory, fresh per row; the provider is scripted. The companion does run on plain Cargo
(`cargo run --locked`), not through kiln, so it does not share the Bazel cache and the first
invocation on a cold fork pays a full build. Do not configure a live provider — a live model cannot be
made to overflow on demand, and a row that waited for one would be judging the provider.

**Two layers.** The scripted layer is the cell that calls the oversized tool and the cell
that finishes: both must be cells the row's session can *execute*, because a foreign cell
never commits and the turn then never reaches a terminal state — the row hangs rather than
failing. The judged layer is everything above it: the outcome's identity, the host's
recovery, and the continued session, none of which are about the dialect at all.

The dialect is fixed: TypeScript is the sole RLM dialect (ADR 0096) and the cells here are
TypeScript. **Do not try to confirm a served dialect from this bundle.** The checkpoint's
`dialect` field is a hardcoded `"typescript"`, not a reading, so confirming it against the
constant it already is proves nothing. No phase or scorecard row below asks for a dialect
reading, and none should until FIG-3165 lands a host-served field that carries one. Cleanup
tickets for this class are named in `RULES.md` (FIG-3055, FIG-3022).

**Fixture honesty.** The oversized payload is a real tool result returned by a real
registered tool through the ordinary tool seam, not a hand-written message injected into
the history. The overflow itself is scripted, because no fixture can make a real model's
window shrink — but it is scripted at two different depths, and the difference is the point:

- The **injected** arm has the provider state `ContextOverflow` directly on an accepted
  response. This reaches the kernel through the Ok-response refinement.
- The **classified** arm has the provider merely *fail*, carrying only the text a real
  provider returns. Lash's own `is_context_overflow_text` is what names the overflow, and
  the failure arrives at the kernel as an error, not a response. This is the path a real
  provider takes.

Requiring both is what keeps the scenario honest. An injected-reason arm alone would prove
the mapping on a path a live provider rarely takes; before FIG-1272 that arm passed while the
classified arm still collapsed into `ProviderError`. The classifier itself
(`is_context_overflow_text`, `refine_terminal_reason_for_context_window`) is unchanged by
FIG-1272 and is not what this scenario judges; the mapping from the classified reason to the
turn outcome is.

## Scenario-specific golden rules

1. **The overflow must arrive mid-turn.** The oversized result must be served by the tool
   inside the turn, and the provider must be asked a second time within that same turn. A
   fixture that overflows on the turn's first request has not reproduced the case the
   ticket is about.
2. **The outcome is its own outcome.** The stop must read `context_overflow`, and the
   public `TurnReport::is_context_overflow` must agree with it. An overflow turn is not a
   success.
3. **Distinction needs two observations.** The control turn must stop as `provider_error`
   on the same harness. One outcome observed alone proves nothing about distinguishability;
   if both arms report the same stop, that is a real-defect stop.
4. **Recovery is plugin-owned (FIG-2950).** The registered standard-compaction
   plugin owns the recovery policy: its `after_turn` trigger records the
   durable marker for a persisted `context_overflow` outcome, and the next
   turn re-derives the pending recovery from that durable state — one bounded
   out-of-band summarizer (the standard compaction prompt with the oversized
   body elided), then the summary and one terminal record appended through
   the plugin's graph seam. The fixture writes no host-side compaction call;
   a host-side `compact_context` in the fixture is the pre-FIG-2950 policy
   and its absence is what this scenario now proves.
5. **The session continues; it is not restarted.** The post-recovery turn runs on the same
   session id and finishes successfully.
6. **Every claim needs observed evidence.** A required outcome with no companion evidence is
   a finding. Any observed contradiction is a real-defect stop; never loosen a gate to make
   a run pass.

## Evidence to inspect

- Companion artifacts from the command above. Each row's `03-observed.jsonl` is backend
  truth; `01-contract-tests.log` is the focused kernel-mapping test.
- The procedure and expected decisions are in this runbook. The companion artifacts are the
  independent behavior evidence; do not score the prose by reading the prose again.
- Save the completed scorecard in the artifact directory. Do not edit the runbook,
  companion, or artifacts during judgment.

## Phase 0 — Contract gate

Require `01-contract-tests.log` to show exactly one passing test
(`context_overflow_response_stops_as_its_own_outcome`), the kernel-level proof that a
`ContextOverflow` terminal reason no longer maps to `ProviderError`.

**Fail if:** the filter matched no test (a `0 passed` line reads green and proves nothing),
or more than one test ran under a filter meant to name one.

## Phase 1 — An honestly mid-turn overflow, on both arms

Read `context_overflow_recovered` (injected) and `classified_overflow_recovered`
(classifier) in the row's `03-observed.jsonl`.

Require, of *each*: `oversized_tool_result_bytes` at least 256 KiB; and `provider_calls` at
least 3, so the turn asked the provider again *after* the tool result landed. Do not assert
anything about `dialect` here; see the dialect paragraph above.

**Fail if:** either arm is missing, the tool result was small, or a turn made a single
provider call.

## Phase 2 — The outcome is its own, and is distinguishable

Require, of *each* overflow arm: `overflow_stop == "context_overflow"`,
`overflow_is_context_overflow == true`, and `overflow_is_success == false`. Require the two
arms to agree, because the path the reason took must not change the outcome. Then read
`provider_error_control` and require `control_stop == "provider_error"`.

**Fail if:** either overflow stop reads `provider_error` (the pre-FIG-1272 behavior), the two
arms disagree, the public accessor disagrees with the serialized outcome, an overflow turn
reports success, or the control turn produces the same stop as an overflow turn.

## Phase 3 — Plugin-owned recovery, both arms

Require, of each overflow arm, `plugin_recovery_pending == true` (the durable
marker the `after_turn` trigger appended rides the overflow turn's own
commit), `plugin_recovery_completed == true` (the plugin's terminal record is
in the committed history), and `plugin_recovery_summary_chars > 0`: the
out-of-band summarizer ran once and its summary is durable, so the recovered
window no longer contains the oversized body.

Two different reads are in play here and the runbook means both, separately:

- `history_messages_after_recovery` is a **read-view** count. After the FIG-3107 frame
  switch the session is resident in the recovery frame and the read view projects only that
  frame, so `1` is the correct, expected value — not evidence that history was lost. Require
  `> 0` and read it as "the recovered window is populated", nothing more.
- The original history, including the oversized body, stays inspectable in the **durable**
  tree, which the emitter reads through a different call (`durable_messages`). That is the
  whole point of the durable record order; recovery never rewrites it. Do not expect the
  read-view count to reflect it.

Also require the recovery frame's own witness, which this bundle carries and the older text
left unscored: `recovery_frame_moved` with `recovery_frame_reason: "compaction"`.

**Fail if:** the plugin never derecorded the marker or terminal record, no summary was
produced, or `recovery_frame_moved` / `recovery_frame_reason: "compaction"` is absent. (The
older clause "the recovery required the fixture's host hand-rolled compaction" is deleted:
there is no host compaction path left in the fixture for a recovery to require, so the clause
could never fire — it described the pre-FIG-2950 world. The frame witness above is its live
successor.)

## Phase 4 — The session continues

Require, of each overflow arm, `continued_is_success == true`,
`continued_is_context_overflow == false`, and a non-null `continued_final_value`, all on the
same `session_id` that arm's overflow turn used. The
scripted finish cell commits a final value rather than assistant text, so
`continued_assistant_message` is null by construction and is recorded for completeness, not
judged.

**Fail if:** the continued turn failed, overflowed again, or ran on a different session.

## Scorecard

Score at gate granularity, one row per independent requirement — a phase that states three
requirements cannot record which of them was observed if it is collapsed to one line.

| Phase | Claim | Verdict | Evidence |
| --- | --- | --- | --- |
| 0 | Kernel maps `ContextOverflow` to its own stop | | `01-contract-tests.log` |
| 1 | Overflow arrives mid-turn on an oversized tool result, injected arm | | `03-observed.jsonl` |
| 1 | Overflow arrives mid-turn on an oversized tool result, refused arm | | `03-observed.jsonl` |
| 2 | The overflow stop is its own outcome, not `provider_error` | | `03-observed.jsonl` |
| 2 | Both arms report the same outcome | | `03-observed.jsonl` |
| 2 | The control turn's stop differs from an overflow turn's | | `03-observed.jsonl` |
| 2 | The public accessor agrees with the serialized outcome | | `03-observed.jsonl` |
| 3 | Recovery marker and terminal record are both durable, both arms | | `03-observed.jsonl` |
| 3 | A summary was produced (`plugin_recovery_summary_chars > 0`), both arms | | `03-observed.jsonl` |
| 3 | `recovery_frame_moved` with `recovery_frame_reason: "compaction"`, both arms | | `03-observed.jsonl` |
| 4 | The continued turn succeeds and does not overflow again, both arms | | `03-observed.jsonl` |
| 4 | It runs on the same `session_id` as its overflow turn, both arms | | `03-observed.jsonl` |

Record the artifact directory and the companion's final line with the scorecard. There is
no dialect to record: it is a compile-time constant, not an observation.
