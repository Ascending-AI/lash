# E2E Runbook Rules

Read this before running any scenario in `runbooks/`. Each runbook links here and does
**not** repeat these rules — it only adds its scenario-specific purpose, golden rules,
phases, and scorecard.

`runbooks/` has **two layers**. Scripted deterministic harnesses
(`runbooks/restate-postgres-workers/`, driven by `just restate-postgres-workers-e2e` and the
`scripts/*-e2e.sh` runners) are gate **evidence**: they boot real infrastructure and assert
exact outcomes, and they stay scripts. Browser runbooks are the **agent-judged semantic
layer** on top: you (the agent) drive the example apps through browser automation and judge
the result with your own reasoning, gating on what the browser surface actually renders.
Keep the layers separate — a runbook never re-implements a scripted harness, and a
scripted harness never asks for judgement.

CLI operator runbooks live in the lash-cli repository's `runbooks/` directory.

These are **agent-driven runbooks**, not scripts. Use judgement freely — but never skip a
scenario's verification gates or the Abort rule below.

## Example coverage matrix

This matrix is the source of truth for the coverage split. **Deterministic CI** means
the repository's repeatable compile, test, and model-check gates. **Full-host CI** means
an infrastructure-backed integration leg; it does not imply that a browser journey was
judged. **Manual judged** is the semantic browser or static-page runbook layer.

| Example | Deterministic CI coverage | Full-host CI coverage | Manual judged coverage |
| --- | --- | --- | --- |
| `agent-service` | `Test docs + build cache` runs `Check workspace (all targets)`; `Test shard ${{ matrix.shard }}/3` runs `Test workspace shard`. | `Functional E2E (agent-service)` runs `agent-service-restate-e2e`, including the Restate ingress and process-workflow live test; it is not a browser journey. | [`agent-service-branching`](agent-service-branching/runbook.md) and [`tictactoe-full-game`](tictactoe-full-game/runbook.md). |
| `agent-workbench` | `Test docs + build cache` runs `Check workspace (all targets)` and the package-scoped workbench check; `Test shard ${{ matrix.shard }}/3` runs `Test workspace shard`. | `Functional E2E (agent-workbench)` runs `agent-workbench-restate-e2e` with Restate and Postgres live tests; it is not a browser journey. | [`workbench-process-lifecycle`](workbench-process-lifecycle/runbook.md), [`workbench-session-resume`](workbench-session-resume/runbook.md), and [`workbench-deferred-tools`](workbench-deferred-tools/runbook.md), plus the other `workbench-*` runbooks. |
| `docs-snippets` | `Test docs + build cache` runs `Check workspace (all targets)`, which compiles the snippet target, and the docs/API checks. | None. `Publish docs` publishes the checked-in static docs; it does not judge a hosted quickstart journey. | [`docs-quickstart`](docs-quickstart/runbook.md). |
| `slack-clone` | `Test docs + build cache` runs `Check workspace (all targets)`; `Test shard ${{ matrix.shard }}/3` runs the workspace tests, including the Slack package tests. | `Functional E2E (slack-clone-full-host)` is token-free and deterministic. The separate `Slack-clone live-model acceptance` workflow is dispatch-only and uses exact nonce/tool/UI oracles around real OpenRouter turns. | [`slack-clone-bot`](slack-clone-bot/runbook.md) and [`slack-clone-mcp-client-depth`](slack-clone-mcp-client-depth/runbook.md). |
| `workflow-graph-roundtrip` | `Test docs + build cache` runs `Check workspace (all targets)`; `Test shard ${{ matrix.shard }}/3` runs workspace tests; `Lint` runs `Check workflow graph model`. | Partial: `Functional E2E (workflow-graph-roundtrip)` runs `workflow-graph-integration-verify` (frontend production build, backend tests, and model check); it does not judge the browser journey. | [`workflow-editor-authoring`](workflow-editor-authoring/runbook.md). |

## Dialect parity is mandatory

Every judged scenario is a two-row acceptance matrix: run it once with the session pinned
to `lashlang` and once with the session pinned to `typescript`. This includes scenarios
whose primary mechanism is dialect-independent. Use a fresh session id, data directory,
ports, trace offset, and artifact directory for each row; never reuse one dialect's
evidence for the other. The machine-readable inventory is
[`parity-matrix.toml`](parity-matrix.toml).

Set `LASH_RUNBOOK_DIALECT` to the row's language id and make the host pass that value in
the RLM session-creation contract. Absence is allowed only for a Lashlang row and must be
recorded as the default substitution. A TypeScript row that receives a Lashlang prompt,
cell tag, execution event, or restored engine id is a contract violation and triggers the
normal Abort/RCA rule. This includes a subagent's prompt: a session tree is one dialect in
v1, and children inherit the parent's, so a TypeScript row whose child session reads a
Lashlang prompt is the same violation. Letting a host pick a different dialect per child
is future work.

The data directory must be fresh per row, not merely per scenario. A session's dialect is
durably pinned at its first commit, so reopening one under a different `LASH_RUNBOOK_DIALECT`
is refused — a store carried over from the other row's dialect fails every route that opens
a session, and carrying one over in the other direction keeps serving the recorded dialect
while the environment claims otherwise. Neither is a battery result; both are a dirty
harness.

Runbook prose predating the parity matrix may say “Lashlang cell/program/source.” Read
that as “the active dialect's cell/program/source” unless it names a stable product API,
artifact filename, trace field, or historical term (for example
`/api/lashlang-graphs` or `lashlang-execution.jsonl`). Prompts ask for outcomes, not
ready-made source, in both rows. Deterministic providers must expose equivalent fixed
programs for both dialect ids; silently feeding their Lashlang program to a TypeScript
row is a failed harness, not a skipped row.

Independent scenario/dialect rows may execute concurrently from the start, subject to the
repository's two-heavy-job limit and each runbook's port/container isolation rules.
Judging is a separate sharded phase over completed evidence bundles, so a judge never owns
or mutates the app it scores. RLM model-calling steps use `gpt-5.6-sol` or newer; record
the actual execution model and judge model on every row, including any substitution.
`python3 scripts/judged_runbook_matrix.py --shard I/N` emits a stable JSON work shard.

## What you're testing

You are testing the **example app's browser surface**, not the model and not your own
browser automation. The scenario is only valid if the rendered page, app API, and durable
state produce the observed result. When those surfaces disagree, the run is void.

## The browser surface (example apps)

Scenarios drive an **example web app** (`examples/agent-service`,
`examples/agent-workbench`, `examples/slack-clone`, and
`examples/workflow-graph-roundtrip`) or the checked-in docs surface used by
`docs-quickstart`. There is no scripted driver for these judged surfaces — browser
automation is the driver, and the docs runbook serves the static page directly. Use
whatever your harness provides: a browser MCP/plugin, Playwright, or similar. If nothing
is pre-wired, the known-good zero-install path is a PEP 723 Playwright script run with
`uv`:

```python
# /// script
# requires-python = ">=3.10"
# dependencies = ["playwright"]
# ///
from playwright.sync_api import sync_playwright
```

`uv run script.py` resolves Playwright into a cached venv and launches the shared
`~/.cache/ms-playwright` Chromium (run `playwright install chromium` once if the launch
reports a missing browser build).

When navigating an example app, use `wait_until="domcontentloaded"` and then an explicit
waiting assertion such as `expect(locator)` or `wait_for_function` on a row count. Never
use `wait_until="networkidle"`: the Workbench holds two NDJSON event streams
open and runs periodic API polls, while runbook drivers also poll `/api/state`, so
network idle is not a reachable readiness condition.

Apply every rule below to the browser surface:

- **Poll, don't sleep** → gate on a waiting assertion with an explicit timeout
  (`expect(locator).to_be_visible(timeout=...)`, or polling an app API until a condition
  holds) — never a fixed sleep to decide async work finished.
- **Gate objectively**, in order of authority: (1) the **rendered page** — text/elements a
  user actually sees, captured as a **named-checkpoint screenshot**; (2) the app's **HTTP
  API** — the runbook names the endpoints that are backend truth; (3) **on-disk artifacts**
  — the example's data dir (SQLite session stores, `trace.jsonl`). The UI and the backend
  must agree: a rendered board the board endpoint contradicts, or an inbox card that
  disagrees with the inbox API, is a **contract violation** → Abort/RCA.
- **Screenshots are evidence, not decoration.** Take one at every checkpoint the runbook
  names, save it under the run's artifact directory, and cite the filename in the
  scorecard. A screenshot alone never passes a gate — pair it with the text or API
  assertion that proves what it shows. Scroll containers hide evidence: scroll the
  transcript/timeline to the newest entry before capturing, or the checkpoint reply sits
  below the fold.
- **Selectors are yours to discover.** Runbooks name UI affordances (the board grid, the
  compose form), not CSS selectors — inspect the served page and pick stable selectors
  yourself; a UI change that breaks an affordance the runbook names is a finding, not a
  reason to guess.

**Real tokens, deliberate runs.** Except for an explicitly documented, dev-only provider
scenario in a runbook, the examples call OpenRouter (and Tavily for web tools) with keys
from the environment / repo `.env`. Browser scenarios are deliberate, token-spending,
and model-nondeterministic unless their runbook names that exception. Gate real-provider
runs on **structural outcomes** (a terminal game state, a message present in an inbox),
never on exact model prose. A missing required key is a harness gap → Abort; do not add an
ad hoc stub. A deterministic provider is valid only when the runbook names its public
environment selector, expected exact output, and dev-only startup warning.

**Boot and teardown are part of the run.** Phase 0 boots the example (`cargo run -p
agent-service`, `just agent-workbench <port>`) and gates on its readiness signal
(`/healthz`, the listening line). Boot via `cargo run` / the `just` recipe **only** —
never launch a `target/debug/*` path directly: this repo redirects builds through
`CARGO_TARGET_DIR`, so a stale in-repo `target/` binary can predate the endpoints a
runbook gates on and fake a contract violation. You own everything you started: end the run — success
or Abort — with the example stopped and any Docker containers it launched torn down
(`just agent-workbench-down <port>`).

`agent-workbench-restart` is a new helper invocation and does not inherit
`AGENT_WORKBENCH_RUN_DIR` or `AGENT_WORKBENCH_DATA_DIR` from the original `up` command.
Export both values in the invoking shell, or repeat the same values on every
`just agent-workbench-restart <port>` command. Otherwise the helper can silently select
different run metadata or a different durable data directory.

For an Abort/RCA, use the app's pipeline — UI event handling / HTTP API / turn or trigger
execution / durable process / store persistence / render — and name the stage the failure
lives in.

## Poll, don't sleep

Turns, triggers, and process work are async and render over several updates. Gate on a
waiting assertion with an explicit timeout (`expect(locator).to_be_visible(timeout=...)`,
or polling an app API until a condition holds) — never a fixed sleep to decide async work
finished. A timeout at a gate is a hard failure → Abort/RCA.

## Don't blind yourself with the fault you inject

Browser runbooks inject faults with shell commands — stop a container, replace a process. While
your driver is inside that command it cannot poll the page, so any state that exists **only**
during the fault is invisible to a driver-side loop: an outage banner, a degraded pill, a
transient affordance. Its absence from your evidence then proves nothing about the app. These
windows are short — a workbench web-process restart is about two seconds — so this is the
normal case, not an edge case.

Move the observation into the page (an interval recording the state into an array the driver
reads afterwards) or launch the injecting command non-blocking. If an affordance must be *used*
during the fault, arm the click in the page too. "The degraded state was never rendered" is a
finding only once you can prove the observer was able to see it.

## Gate objectively before you judge

Prefer an objective signal over eyeballing. In order of authority:

1. **Rendered page** — text/elements a user actually sees, captured as a named-checkpoint
   screenshot.
2. **App HTTP API** — the runbook names the endpoints that are backend truth.
3. **On-disk artifacts** — the example's data dir, including SQLite session stores and
   `trace.jsonl`.

Run the structural gate **before** judging behavior. If the objective signal is missing,
the failure is upstream of anything you would judge — Abort/RCA, don't score the vibe. The
UI and backend must agree; a rendered board the board endpoint contradicts, or an inbox
card that disagrees with the inbox API, is a contract violation → Abort/RCA.

## Three-layer cross-check (workbench scenarios)

Self-consistency inside one layer is not evidence. For every scenario step that changes the
conversation, reconcile all three layers before scoring it:

1. **Rendered DOM** — the rows a user actually sees, counted per role.
2. **Durable state** — the session graph's committed messages plus the app's own projection
   surfaces (`/api/state`, the product-event log).
3. **Logs** — the workbench trace, counted as executions (one completed turn per submitted
   send or wake).

Reconcile them **pairwise**, as counts and identities, not as a vibe: the same step must
produce the same number of user rows, assistant rows, committed messages, projected
messages, and turn executions across all three. **Any pairwise mismatch is a FAIL**, even
when each layer is internally consistent — a duplicated render over a single durable
message and a doubled durable commit under a single execution are both projection defects,
and only the cross-check separates them. When they disagree, record which layers agreed and
which did not; that split is the diagnosis, so never normalize it away.

One tool-level split is deliberate and must be scored as agreement, not an Abort: when a
leaf provider returns successfully but a declared `shell.write` or `processes.cancel`
command is refused during journal-first realization, the immutable `ToolAttempt` frame and
its per-attempt trace row retain the provider's pre-realization value. The recorded typed
intent outcome and the projected turn/API/DOM tool result carry the refusal. Require the
same call identity, the exact typed refusal code/message, and one attempt plus one intent
outcome; any different split, duplicate, missing row, or disagreement among the projected
turn/API/DOM surfaces remains a contract violation → Abort/RCA.

## When to STOP (Abort triggers)

Stop immediately on **any** of:

- a browser automation command error or non-zero driver exit;
- a waiting assertion or API-poll timeout at a gate;
- the example app exiting unexpectedly before a gate;
- a **contract violation** — the rendered page, app API, and on-disk state disagree;
- an assertion that contradicts the scenario's answer key.

Do not push through, do not paper over, do not attempt a fix as part of the run.

## How to REPORT

**On abort — RCA, then stop:**
1. **Stop.** Do not continue the scenario.
2. **Capture evidence** — the failing automation command and its error; the last rendered
   page and named-checkpoint screenshot; app status; relevant API response and on-disk
   artifacts; and the exact gate that failed.
3. **RCA** — symptom → the app stage it broke at (UI event handling / HTTP API / turn or
   trigger execution / durable process / store persistence / render) → root cause → the
   evidence that proves it. Never stop at "the assertion timed out."
4. **Report and stop.** This is a diagnosis, not a repair. A divergence between an observed
   behavior and CONTEXT.md or the docs is reported as a finding — **do not** edit the doc
   or the code to make the run pass.

**On success — score, don't vibe:** for each scored item, name the **specific rendered
text or element** (or API / on-disk fact) the gate matched — no credit for vibes. Mark the
objective gate (page / API / disk) separately from any judged behavior. Fill the
scenario's scorecard verbatim.
