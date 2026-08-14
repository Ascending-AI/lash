# E2E Scenario: RLM Cell Boundary Ownership

> **Read [../RULES.md](../RULES.md) first**. This scenario uses real provider
> tokens and adds only the RLM cell-boundary gates.

**Purpose.** Prove that a live Workbench RLM turn sends no provider stop
sequences, records caller-stop suppression honestly, accepts the first complete
Lashlang cell, and never executes content after that boundary.

## Scenario-specific golden rules

1. Use a fresh Workbench data directory and a caller generation policy with a
   unique non-empty stop sequence. Never use ports 3056 or 3057.
2. Treat the extended trace as wire truth. The provider request body must omit
   stop sequences (or carry the dialect's empty representation), while the
   completed LLM outcome and attempt ledger both report
   `suppressed_protocol_owned`.
3. Treat the trajectory and tool activity as execution truth. Only code inside
   the first complete paired cell may appear there; prose or another cell after
   the first close must not execute.
4. Before the live run, require the automated chunk-parity law
   `accepted_cell_is_independent_of_split_immediately_after_end_tag` and the
   streamed scenario `rlm_protocol_scenario_plugin_stream_mask_splices_chunk_spanning_cell_for_reextraction`
   to pass. Those deterministic laws own adversarial provider split coverage;
   do not infer byte-level chunking from a browser trace.

## Phase 0 — Boot and baseline

Boot a fresh Workbench on an available port other than 3056/3057 with extended
tracing enabled. Poll `/healthz`, open the browser, record the trace byte offset,
and capture `00-ready.png`. Abort if the rendered session id and `/api/state`
session id disagree.

## Phase 1 — Exercise a first-cell boundary

Submit a prompt that asks the RLM to compute a small value in one Lashlang cell
and explicitly asks it to explain another possible computation after the code.
Poll until the turn settles. Capture the fully scrolled transcript and activity
rail as `01-first-cell.png`.

Require one successful first-cell execution. If the model naturally emits a
second cell or trailing prose, require that it produces no second execution. If
it does not, record that the live suffix-discard branch was not sampled; the
deterministic two-cell laws remain the authoritative gate.

## Phase 2 — Reconcile wire, evidence, and execution

From trace records after the Phase 0 offset, save the provider request and
completed LLM record as `02-boundary-trace.json`. Require:

- the request contains no caller or Lashlang delimiter stop sequence;
- response and attempt dispositions both say `suppressed_protocol_owned`;
- the response has a request body and typed execution evidence;
- usage reported before a protocol abort appears in the response and attempt;
- exactly the first accepted cell appears in the trajectory/tool activity.

Any disagreement is a contract violation. Capture the trace excerpt, rendered
activity, and `/api/state`, then Abort/RCA.

## Phase 3 — Teardown and score

Stop everything started by this run and confirm teardown. Record PASS/FAIL for:

| Gate | Result | Evidence |
| --- | --- | --- |
| Zero wire stops | | `02-boundary-trace.json` |
| Honest suppression disposition | | `02-boundary-trace.json` |
| First cell executed once | | rendered activity + trajectory |
| Suffix never executed | | rendered activity + trajectory |
| Request, typed abort evidence, and observed usage retained | | `02-boundary-trace.json` |
| Automated adversarial split laws green | | exact test commands and exits |
