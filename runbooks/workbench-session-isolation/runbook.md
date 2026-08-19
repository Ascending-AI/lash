# E2E Scenario: Workbench Concurrent-Session Isolation

> **Read [../RULES.md](../RULES.md) first** — especially polling, named-checkpoint
> screenshots, real-token use, port-derived stacks, the isolation-void rule below,
> Abort/RCA, and teardown ownership.

**Purpose.** Drive two durable sessions concurrently through one workbench process and
deliberately hunt for cross-session leaks in transcripts, trigger registrations, trigger
delivery, and durable process projections. The two sessions use the same trigger source
and deliberately similar process names so superficial partitioning cannot pass.

**Real tokens.** Both sessions call OpenRouter; the overlap prompts also use Tavily to
make concurrent turns observable. Gate on literal operator markers and structural API
state, never exact assistant prose.

## Scenario-specific golden rules

1. **Two explicit session tabs, one process.** Use **new session tab** twice and retain
   both generated `session_id` query parameters. Do not boot a second workbench.
2. **Scope every session API read.** Query `/api/state`, `/api/triggers`, and `/api/work`
   with that tab's `?session_id=...`. An unscoped `/api/work` response is runtime-wide
   and is not isolation evidence.
3. **Use confusable fixtures.** Both registrations must have the same derived display
   name, `subscription_key`, button color, source type/configuration, and process label.
   Only `subscription_id`, process ids, and session ownership distinguish them.
4. **Prove non-membership.** Presence in the expected session is only half a gate. Every
   marker, registration `subscription_id`, and process id must also be absent from the
   other session's rendered/API surface.
5. **Isolation-void rule.** Any foreign transcript marker, foreign registration id,
   foreign process id, UI/API disagreement, missing session query, or collapse of both
   tabs onto one rendered session id voids the run immediately → Abort/RCA. Do not
   continue collecting later evidence from contaminated state.

## Working material

- Require `OPENROUTER_API_KEY` and `TAVILY_API_KEY`. Boot one empty, port-isolated stack:
  `AGENT_WORKBENCH_DATA_DIR=<fresh-tmp> AGENT_WORKBENCH_OPEN=0 just agent-workbench <port>`.
  Gate `GET /healthz` → 200. Teardown on success or Abort:
  `just agent-workbench-down <port>`.
- UI affordances: **new session tab**, rendered session id, chat composer/transcript,
  running/idle pill, Red/Blue trigger buttons, registrations rail, and work rail.
- Scoped backend truth for session `<S>`:
  `GET /api/state?session_id=<S>`, `GET /api/triggers?session_id=<S>`,
  `POST /api/button-trigger?session_id=<S>`, and `GET /api/work?session_id=<S>`.
  `GET /api/work/{process_id}/await` supplies terminal evidence after ownership is proven.
- The button-trigger request body is exactly `{"button":"Red"}` or
  `{"button":"Blue"}`; the `button` value is case-sensitive, so lowercase `red` or
  `blue` returns HTTP 422.
- Deterministic companion gate:
  `cargo test -p agent-workbench concurrent_sessions_isolate_transcripts_triggers_and_processes`.
  It runs simultaneous turns and simultaneous session-scoped occurrences against shared
  stores, then asserts transcript non-membership and disjoint process projections.

Save all named API responses beneath the run's artifact directory. Use `A` and `B` in
filenames only as aliases; record the full generated session ids in `00-sessions.json`.

## Phase 0 — Boot two isolated tabs

Boot and gate `/healthz`. From the landing tab, activate **new session tab** twice and
capture the two opened pages as Tab A and Tab B. Record each URL `session_id`, rendered
session id, and `/api/state.settings.session_id`; require all three to agree within each
tab and require A ≠ B. Both tabs must report the same server origin. Save
`00-sessions.json`; screenshot `00-tab-a.png` and `00-tab-b.png`.

Close or ignore the original unscoped landing tab. Every remaining interaction and API
request must target A or B explicitly.

## Phase 1 — Register confusable triggers independently

In each tab, ask for the same outcome: register a trigger named `shared-blue-watch` for
the Blue host button that starts a durable process labeled `mirror_job` and records the
button occurrence (Lashlang process identifiers cannot contain hyphens, so the rendered
process label is `mirror_job`). Submit the two registration turns concurrently.

Poll both tabs until idle. Save `GET /api/triggers?session_id=A` and the B equivalent as
`01-triggers-a.json` and `01-triggers-b.json`. Require exactly one enabled registration
per session. Require identical derived display names, `subscription_key` values, source
types, and source configurations, but distinct `subscription_id` values. Same-name,
same-key registrations across the two tabs are correct: the owner scope participates in
`subscription_id`, so neither equality is evidence of a leak. Require each registrations
rail to render the common target and trigger source (e.g. `mirror_job ← button.pressed`),
the common trigger key, the registration alias, and only its own truncated id. Screenshot
`01-trigger-a.png` and `01-trigger-b.png`.

Perform the first leak hunt now: A's `subscription_id` must be absent from B's API/DOM,
and B's `subscription_id` absent from A's. Do not use the shared name or
`subscription_key` for this check. Any id leak invokes the isolation-void rule.

## Phase 2 — Run two turns concurrently and hunt transcript leaks

Prepare distinct literal markers `FIG425-ISO-A-<run-id>` and
`FIG425-ISO-B-<run-id>`. In each tab, submit a prompt that includes its marker, requests
one current fact via `web.search`, and asks that the marker be repeated in the answer.
Trigger the two submissions concurrently through the browser driver.

Poll until both scoped `/api/state` responses simultaneously contain one active turn;
save that overlap as `02-active-a.json` and `02-active-b.json`. Require the active turn
addresses to be distinct. If no overlap is observed before the explicit timeout, the
concurrency gate failed → Abort/RCA; sequential completion earns no credit.

Then poll both tabs to idle. Save `02-settled-a.json` and `02-settled-b.json`. Require:

- A's UI/API transcript contains marker A and not marker B;
- B's UI/API transcript contains marker B and not marker A;
- each transcript's rendered ordered rows agree with its scoped state response; and
- neither trigger list changed during the turns.

Scroll each transcript to its newest row and capture `02-transcript-a.png` and
`02-transcript-b.png`. Apply the isolation-void rule before continuing.

## Phase 3 — Fire the same source concurrently

Record the scoped process-id sets (normally empty). Activate the Blue trigger button in
both tabs concurrently. Poll each scoped `/api/work` until it gains exactly one new
`mirror_job` process. Save `03-work-a.json` and `03-work-b.json`; require the new process
ids to be distinct.

Await both ids with `/api/work/{process_id}/await` and require terminal success. Refresh
the scoped work responses and require:

- A contains A's process id and not B's;
- B contains B's process id and not A's;
- each rail renders the same sole new process as its scoped API;
- both process labels are `mirror_job`; and
- both trigger registrations remain present and owned by their original session.

The same source/configuration and same label are deliberate: matching by either must not
broadcast delivery or projection across sessions. Screenshot `03-work-a.png` and
`03-work-b.png` with the terminal rows visible.

## Phase 4 — Final bidirectional leak matrix

Save fresh scoped state, trigger, and work responses for both sessions. Complete every
cell; a single failed non-membership check invokes the isolation-void rule.

| Surface | Tab A must contain | Tab A must exclude | Tab B must contain | Tab B must exclude |
|---------|--------------------|--------------------|--------------------|--------------------|
| Transcript | marker A | marker B | marker B | marker A |
| Triggers | subscription id A | subscription id B | subscription id B | subscription id A |
| Work | process A | process B | process B | process A |

Screenshot the complete A surface as `04-final-a.png` and B as `04-final-b.png`. Save the
matrix inputs as `04-state-*.json`, `04-triggers-*.json`, and `04-work-*.json`.

## Phase 5 — Teardown and score

Run `just agent-workbench-down <port>` and confirm the one workbench process and its
port-derived Restate container are gone.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Two sessions/one process | distinct URL/rendered/API ids on one origin | | `00-sessions.json`, `00-tab-*.png` |
| Trigger ownership | same-name, same-key registrations have distinct ids and no cross-listing | | `01-triggers-*.json`, `01-trigger-*.png` |
| Concurrent turns | both scoped states were active at once with distinct addresses | | `02-active-*.json` |
| Transcript isolation | each marker present locally and absent remotely in UI/API | | `02-settled-*.json`, `02-transcript-*.png` |
| Trigger delivery isolation | simultaneous same-source fires start one local process each | | `03-work-*.json` |
| Process projection isolation | process ids disjoint and absent from the foreign rail/API | | `03-work-*.png`, `04-work-*.json` |
| Final leak matrix | all three bidirectional non-membership rows pass | | `04-final-*.png`, `04-*.json` |
| UI/API agreement | transcript, trigger, and work surfaces agree in both tabs | | all screenshots + scoped API artifacts |

**Aggregate:** did two concurrently active sessions remain disjoint at every durable and
rendered seam even when their trigger sources and process labels were intentionally
indistinguishable?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
