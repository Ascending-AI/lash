# E2E Scenario: RLM Cell Boundary Ownership

> **Read [../RULES.md](../RULES.md) first**. This scenario uses real provider
> tokens and adds only the RLM cell-boundary gates.

**Purpose.** Prove that a live Workbench RLM turn sends no provider stop
sequences, records stop-sequence disposition honestly (`not_requested`), accepts
the first complete Lashlang cell, and never executes content after that boundary.

## Scenario-specific golden rules

1. Use a fresh Workbench data directory. Never use ports 3056 or 3057. The
   Workbench exposes **no caller stop-sequence control**, so do not try to
   configure one: there is no UI, API, or environment seam for it, and a row
   that claims to have set one is fabricating its setup.
2. Treat the extended trace as wire truth, and gate on what the host can
   actually show you. Two limits are structural, not defects, and a gate that
   ignores either can never fire:
   - **Disposition.** `suppressed_protocol_owned` is recorded only when a
     protocol suppresses a caller's *non-empty* stop list. With no caller stop
     sequence there is nothing to suppress, so the honest report through this
     host is `not_requested`. Require that, and require it to be present rather
     than absent.
   - **Request body JSON.** The trace retains exact request JSON only up to
     `MAX_PROVIDER_REQUEST_BODY_JSON_BYTES` (2 KiB); an RLM system prompt is an
     order of magnitude larger, so every real row carries
     `body_json_omitted_reason: "size_limit"` with `body_len` and
     `body_sha256`. Gate on those three. Do not treat the omission as a defect
     and do not raise the bound to make a gate fire: the cap exists so every
     JSONL record and OTEL attribute is not inflated by a full prompt on every
     request, which is a durable cost paid by every user of tracing.

   The two properties this scenario cannot observe live — that a caller's stop
   sequence is suppressed rather than sent, and the exact absence of a stop
   field on the wire — are owned by deterministic laws instead:
   `protocol_stop_suppression_updates_response_and_attempt_ledger`
   (`crates/lash-core`, the response-and-attempt ledger) and the streamed
   scenario in golden rule 4. Run them and record the exact commands and exit
   codes, exactly as that rule already requires.
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

- the request carries no stop sequence the host could have set: the Workbench
  sets none, so `stop_sequences` is absent or empty wherever the record shows
  request options;
- response and attempt dispositions both report the stop-sequence disposition
  this host honestly produces — `not_requested` — and both are present;
- the request record carries `body_len`, `body_sha256`, and, for any real RLM
  prompt, `body_json_omitted_reason: "size_limit"`; a present `body_json` is
  acceptable only if `body_len` is genuinely under 2 KiB;
- the response has typed execution evidence;
- usage reported before a protocol abort appears in the response and attempt;
- exactly the first accepted cell appears in the trajectory/tool activity.

Any disagreement is a contract violation. Capture the trace excerpt, rendered
activity, and `/api/state`, then Abort/RCA.

## Phase 3 — Teardown and score

Stop everything started by this run and confirm teardown. Record PASS/FAIL for:

| Gate | Result | Evidence |
| --- | --- | --- |
| Zero wire stops | | `02-boundary-trace.json` |
| Honest stop disposition (`not_requested`, present) | | `02-boundary-trace.json` |
| Request body accounted for (`body_len` + `body_sha256`, omission reason when over the cap) | | `02-boundary-trace.json` |
| Caller-stop suppression law green (deterministic; unreachable live) | | exact test command and exit |
| First cell executed once | | rendered activity + trajectory |
| Suffix never executed | | rendered activity + trajectory |
| Typed abort evidence and observed usage retained | | `02-boundary-trace.json` |
| Automated adversarial split laws green | | exact test commands and exits |

---

## History

- **FIG-1306 / FIG-1402**: Re-anchored unreachable live gates onto what the Workbench
  can honestly observe: gated on `not_requested` stop disposition rather than unreachable
  `suppressed_protocol_owned` (since the Workbench exposes no caller stop-sequence controls),
  gated on `body_len` + `body_sha256` and size-limit omission rather than requiring full
  request JSON when exceeding `MAX_PROVIDER_REQUEST_BODY_JSON_BYTES`, and delegated caller-stop
  suppression to deterministic laws per ADR 0047 disposition honesty.
