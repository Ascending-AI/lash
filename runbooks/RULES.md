# E2E Runbook Rules

Read this before running any scenario in `runbooks/`. Each runbook links here and does
**not** repeat these rules — it only adds its scenario-specific purpose, golden rules,
phases, and scorecard.

`runbooks/` has **two layers**. Scripted deterministic harnesses
(`runbooks/rlm-smoke/` and the `scripts/*-e2e.sh` runners) are gate **evidence**: they boot real hosts or infrastructure
and assert exact outcomes, and they stay scripts. Runbooks are the **agent-judged semantic
layer** on top: you (the agent) shift browser scenarios or inspect a deterministic companion's
artifact bundle, then judge the observed behavior with your own reasoning. A deterministic
companion runbook must carry the procedure and expected decisions it asks the judge to apply;
reading a separate copy of those expectations is not independent evidence.
Keep the layers separate — a runbook never re-implements a scripted harness, and a
scripted harness never asks for judgement.

These are **agent-driven runbooks**, not scripts. Use judgement freely — but never skip a
scenario's verification gates or the Abort rule below.

## Example coverage matrix

This matrix is the source of truth for the coverage split. **Deterministic CI** means
the repository's repeatable compile, test, and model-check gates. **Full-host CI** means
an infrastructure-backed integration leg; it does not imply that a browser journey was
judged. **Manual judged** is the semantic browser or artifact-judgment runbook layer.

| Example | Deterministic CI coverage | Full-host CI coverage | Manual judged coverage |
| --- | --- | --- | --- |
| `agent-workbench` | `Test Buck2 partition tail` runs its unit tests on trusted non-PR events; `Test Buck2 partition` covers them through its affected selection when a trusted pull request touches the package, and `Test Cargo workspace partition` runs them on untrusted pull requests. `Feature lanes (${{ matrix.shard }}/4)` compiles its feature variants and runs their tests. | No live engine leg until L9h (FIG-5186) rebuilds it. | [`workbench-process-lifecycle`](workbench-process-lifecycle/runbook.md), [`workbench-session-resume`](workbench-session-resume/runbook.md), and [`workbench-deferred-tools`](workbench-deferred-tools/runbook.md), plus the other `workbench-*` runbooks. The deterministic, operator-only [`workbench-attachment-reclamation`](workbench-attachment-reclamation/runbook.md) rehearsal of the condemn/vacuum/reclaim levers is inventoried separately and is never a judged browser row: the workbench renders no affordance for either lever, so `curl` against the admin route is the operator surface. |
| `rlm-smoke` | `Test Buck2 partition` runs `rlm-smoke`'s unit tests on trusted events (the package sits in the core suite) and `Test Cargo workspace partition` covers untrusted pull requests; the focused tests prove path, symlink, and command jail refusals. | `just rlm-smoke-e2e` runs the three separately funded matrix rows `rlm-smoke-file-edit-bugfix`, `rlm-smoke-missing-helper-file` and `rlm-smoke-config-contract-edit` against exact shell oracles after live OpenRouter turns; this one line describes the gate, the three matrix rows are the inventory. It is a local/manual paid gate, not per-PR CI. | None. These are scripted deterministic-oracle rows, never judged browser rows. |
| `toolbench` | `Test Buck2 partition tail` runs its unit tests on trusted non-PR events; `Test Buck2 partition` covers them through its affected selection when a trusted pull request touches the package, and `Test Cargo workspace partition` runs them on untrusted pull requests. | None. The benchmark is an operator-run paid pass against real OpenRouter models; CI does not fund it. | None. It is a self-scoring measurement harness: its `results-file` task rows are the evidence, not a judged browser journey. |
| `workflow-graph-roundtrip` | `Test Buck2 partition tail` runs its unit tests on trusted non-PR events; `Test Buck2 partition` covers them through its affected selection when a trusted pull request touches the package, and `Test Cargo workspace partition` runs them on untrusted pull requests. | None. Its one law drives the HTTP surface over SQLite in the unit partition; nothing scripted judges the browser journey. | [`workflow-editor-authoring`](workflow-editor-authoring/runbook.md). |

Directories under `examples/` with no row: `shared` is a library the hosts import, not
a runnable host; `integrator-contract` is a compile-only signature witness, not an
example a host would read; `typescript-host-flows` is a collection of TypeScript
cells exercised by its own judged row. The sibling witness `transcript-contract`
lives under `crates/`, beside the file it checks.

## One judged row per scenario, served in TypeScript today

[ADR 0096](../docs/adr/0096-typescript-is-the-sole-rlm-dialect.md) defines one IR
and VM with extensible dialects. TypeScript is the only shipped dialect today.
Each judged scenario **that runs an RLM session** therefore has one row served
in `typescript`. The machine-readable inventory is
[`judged-matrix.toml`](judged-matrix.toml), whose top-level `language` key names
the dialect these rows exercise. Future dialects own their semantics and
coverage; they need not duplicate another dialect's scenarios.

Each row still gets a fresh session id, data directory, ports, trace offset, and artifact
directory. A carried-over store serves a previous row's processes, leases and
trace offsets, which is mislabeled evidence.

Each emitted row carries a `label`, which is both its artifact directory and a claim its
evidence has to support. `typescript` says the row opened an RLM session served in TypeScript. A
scenario that **opens no RLM session** can make no such claim; those sit in
`no_rlm_session_only` and emit one row each labelled `standard` — the mode, not a
language. A standard-mode host using `LashCore::standard_builder` is one example; the
predicate is whether the scenario opens an RLM session, and the claim is checkable in its
runbook binary or driver.

There is nothing to pin. The RLM session-creation contract carries no language, so
`LASH_RUNBOOK_DIALECT` is inert: nothing reads it, and the companions that still
name it reject it outright. A row that served anything but a
TypeScript prompt, cell tag, execution event or restored engine id is still a contract
violation that triggers the normal Abort/RCA rule — it just cannot be caused by
configuration any more. This includes a subagent's prompt: children read the same
TypeScript prompt their parent does.

Confirm the served language from the row's **own evidence** — prompt, cell tag, execution
events — never from the environment. The environment is what you asked for; the evidence
is what you got, and the gap between them is the whole reason the label exists.

Runbook prose predating this ruling may say "Lash VM cell/program/source". Read that as
the TypeScript cell and its source unless it names a stable product API, artifact
filename, trace field, or historical term (for example `/api/lash-vm-graphs` or
`lash-vm-execution.jsonl`), which keep their spellings because they name the IR and the
VM, which are not retired. Prompts ask for outcomes, not ready-made source. A
deterministic provider must expose a TypeScript program its scenario can actually run;
serving a program in the retired surface is a failed harness, not a skipped row.

Independent scenario rows may execute concurrently from the start, subject to the
repository's two-heavy-job limit and each runbook's port/container isolation rules.
Judging is a separate sharded phase over completed evidence bundles, so a judge never owns
or mutates the app it scores. `python3 scripts/judged_runbook_matrix.py --shard I/N` emits a
stable JSON work shard. The matrix currently expands to **24 rows**: 21 RLM scenarios, two
no-RLM-session rows, and one composite that exercises host-language surface. The arithmetic
is asserted by `scripts/test_judged_runbook_matrix.py`, so a reclassification cannot leave
this number stale without turning CI red.

Score a `cargo test`/`cargo nextest` run by its **passed count**, never by its exit code. A
filter that matches nothing exits `0` and prints `0 passed; N filtered out`, which reads as
a green gate and is evidence of nothing. Two shards of an earlier round were reported green
on exactly that.

## Execution tiers

A row's execution model is a **claim about what the row is testing**, not a uniform quality
floor. The earlier blanket `gpt-5.6-sol` execution floor bought a frontier model for rows
whose every gate is a row count, an id, or a byte comparison — evidence any competent driver
produces identically. `runbooks/judged-matrix.toml` therefore carries a `tier` and the
concrete `model` slug per scenario, and the emitted shard carries both on every row.

- **`deterministic`** — the row makes **no provider network call**. A scripted or in-process
  fixture provider supplies every reply, so the model's choices are not the judged subject at
  all. A deterministic row is only honest when the runbook names its public environment
  selector, expected exact output, and dev-only startup warning (the real-token rule below).
  The matrix refuses a deterministic scenario that names a real model slug.
- **`economy`** — `deepseek/deepseek-v4-flash`, or `deepseek/deepseek-v4-pro` where the row
  needs the driver to author a non-trivial durable program or discover an affordance rather
  than follow an instruction the runbook states outright. Every gate is still a count, an id,
  a durable fact, or a literal the operator supplied.
- **`frontier`** — `openai/gpt-5.6-sol`. Reserved for rows where model **behaviour is the
  judged subject**, so a weaker driver changes the verdict rather than the prose: the organic
  `continue_as` lever, first-shot codemode fluency, and the pinned-program-shape rows whose
  documented failure mode is an Abort rather than a retry.

Four rules keep the tiers honest:

1. **The tier is not a licence to weaken a gate.** If a gate only passes at `frontier`, the
   row is `frontier`; do not retune the answer key downward to fit a cheaper driver.
2. **Record the served model from the row's own evidence**, never from the environment — the
   same rule the language label already carries. Any substitution (a slug that is unavailable, a
   tier raised mid-row after a repeated model failure) is recorded on the row's scorecard with
   the reason.
3. **A tier change is a matrix change.** Running a row at a model the matrix does not name for
   it produces mislabeled evidence in the same way a carried-over data directory does. Move
   the scenario's tier in `judged-matrix.toml` first, in its own commit, with the reason.
4. **A row served by the dev provider is `deterministic`, or says which phases are not.** The
   model-slug check reads only the matrix, so it cannot see a row whose every phase is
   answered in-process while its tier buys a real driver: the slug is never requested, and the
   scorecard then names a model that produced none of the evidence. A runbook naming
   `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO` is therefore either tier `deterministic` or carries
   `deterministic_phases` saying which phases call a provider. `judged_runbook_matrix.py`
   refuses the rest.

Where a runbook is half mechanics and half behaviour, the mechanical half runs deterministic
and only the residue is funded: `judged-matrix.toml` records that split per scenario in
`deterministic_phases`.

The judge is separate and unchanged: `judge_model_floor` stays `gpt-5.6-sol`. A cheap driver
producing the evidence does not license a cheap reader of it.

## What you're testing

You are testing the **example app's browser surface**, not the model and not your own
browser automation. The scenario is only valid if the rendered page, app API, and durable
state produce the observed result. When those surfaces disagree, the run is void.

## The browser surface (example apps)

Scenarios shift an **example web app** (`examples/agent-workbench` or
`examples/workflow-graph-roundtrip`). There is no scripted driver for these judged
surfaces — browser automation is the driver. Use
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
scenario in a runbook, the examples call OpenRouter (web tools ride the keyless Parallel
Search MCP server) with keys
from the environment / repo `.env`. Browser scenarios are deliberate, token-spending,
and model-nondeterministic unless their runbook names that exception. Export the model the
row's tier names (`OPENROUTER_MODEL`, and confirm it from the row's own evidence rather than
the environment). Gate real-provider runs on **structural outcomes** (a terminal game state,
a message present in an inbox), never on exact model prose. A missing required key is a
harness gap → Abort; do not add an ad hoc stub. A deterministic provider is valid only when the runbook names its public
environment selector, expected exact output, and dev-only startup warning.

**Boot and teardown are part of the run.** Phase 0 boots the example (`cargo run -p
agent-workbench --profile judged`, `just agent-workbench <port>`) and gates on its readiness
signal (`/healthz`, the listening line). Boot via `cargo run` / the `just` recipe / the
launcher script **only** — never launch a `target/*/…` or `buck-out/…` path directly:
this repo redirects Cargo builds through `CARGO_TARGET_DIR` and writes the judged and the
ordinary Buck2 configuration to one output path, so a binary picked up by hand can predate
the endpoints a runbook gates on, or carry the wrong geometry, and fake a contract
violation. A caller that genuinely has a binary to reuse passes it as
`AGENT_WORKBENCH_BIN` and lets the launcher own the boot. You own everything you started: end the run — success
or Abort — with the example stopped and any Docker containers it launched torn down
(`bash scripts/agent-workbench-dev.sh down --port <port>`, wrapped by
`just agent-workbench-down <port>`).

**The `just` workbench recipes do not export `CARGO_TARGET_DIR`.** `just agent-workbench
<port>`, `-down`, `-restart` and the rest all exist and all run
`scripts/agent-workbench-dev.sh`, but a lane that invokes them without sourcing the fork's
`env.sh` first builds into the default target directory instead of the fork's warm one. Source
`env.sh`, or call `bash scripts/agent-workbench-dev.sh <verb> --port <port>` directly — which
is the form every runbook here spells out.

**The launcher's data-ownership lock is box-wide, not per-port.** Lifecycle commands
(`up`/`down`/`restart`) serialize on `/tmp/lash-agent-workbench-$UID/data-ownership.lock`,
shared across every port and every checkout for the user. A concurrent lifecycle command
anywhere on the box makes an unrelated row's command fail with `another launcher lifecycle
command is updating application data ownership` — a message that reads like data corruption
and is not one. Retry the command; it succeeds as soon as the other one releases the lock. A
shift running rows in parallel should expect this and not treat it as an Abort.

**The judged build geometry is the shipping one.** Every judged host is built with no
`testing` feature on any host dependency and `debug-assertions`/`overflow-checks`
compiled out. The `just` recipes and the `scripts/*-dev.sh` launchers already ask for
that geometry; the one host you boot by hand (`agent-workbench`) needs `--profile judged`
typed, and a row booted without it is invalid evidence — rerun it.

The geometry has two spellings, because `agent-workbench` is no longer built by Cargo.
`scripts/agent-workbench-dev.sh` builds `//examples/agent-workbench:agent-workbench`
through Buck2 (`kiln build --config=judged`), so every checkout on the box shares one
action cache instead of compiling the workspace again into its own target directory, and
it builds before it takes any launcher lock rather than stalling every other stack's boot
behind its own compile. `--config=judged` in the Buck2 target configuration is the rustc-flag spelling of
`[profile.judged]` — without it the label would keep the debug assertions rustc turns on
by default at `-C opt-level=0`. `AGENT_WORKBENCH_BIN=<path>` skips the build and launches
that binary instead, which is how a driver that boots row after row pays for one build.
Building a workbench with the `provider-wire-fixtures` feature
(`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion`) still goes through Cargo
and `--profile judged`: the feature turns on an optional dependency outside the single
workspace feature resolution the generated BUCK files describe, so no Buck2 label builds
that shape.

This exists because a judged run scores what the host ships. A `dev` build turns
in-contract outcomes into opaque failures: an exhausted Lash VM execution bound is a
`Policy` observation the runtime hands back to the agent, but under the workbench's old
`testing`-feature build it tripped an assertion inside the effect task and the turn
surfaced as `effect_panicked` → "turn could not be completed" — a permanently
unjudgeable row that says nothing about the product. Debug-assert and `testing`-feature
coverage is real, and it is the unit/law lane's job: `cargo test` / `cargo nextest` keep
the `test` profile and keep every one of those checks armed.

So a panic in a judged host is now a finding, not background noise. `panicked at` in a
host log is an Abort/RCA — it is a genuine crash in shipping code, not a development
self-check firing.

`scripts/check_judged_build_geometry.py` checks the shipping profile settings,
the absence of `testing` on host runtime dependencies, the judged boot profile,
its Buck2 rustc flags, and builds before workbench launcher locks.

## Agent Workbench lifecycle constraint (FIG-1164)

`just agent-workbench-restart <port>` (`scripts/agent-workbench-dev.sh restart`) is the
non-destructive same-configuration replacement (FIG-3035). It replaces **only** the Workbench
process, at the same address and store backend, and keeps any managed Postgres and the
application data directory. This is the command a phase that proves durable state survives a
process replacement uses.

It is verified and is **not** blocked: FIG-3035 landed it, the 2026-09-15 judged shift
executed it on every workbench row that needs a process replacement, and every continuity gate
held. A phase that still reads "blocked by FIG-1164" is stale documentation, not a live
constraint. It refuses before stopping anything unless the launcher's own run metadata proves it owns a
matching stack at exactly the current settings. An interrupted replacement is retryable with
the same command.

`just agent-workbench-reset <port>` (`restart --reset-dev-state`) is unchanged and remains an
explicitly destructive recovery command: it clears the stack's durable state and application
data, and works only for a wholly launcher-owned disposable stack. Legacy,
external, mixed, or ambiguous stacks are refused. Never substitute it for a
process-replacement phase, because deleting the evidence cannot prove persistence.

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
fault windows may be short, so this is the normal case rather than an edge case. A Workbench
process replacement is one such command; the lifecycle section above names it.

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

## Transcript projection gate (workbench scenarios)

The deterministic transcript gate reconciles graph nodes, rendered rows,
`/api/state`, and quiescent rendered DOM pairwise. It covers the former
Surfaces A–E, including reasoning, successful and failed code, nested tool
summaries, terminal values, retry retraction, attachments, and reload.
`scripts/check_transcript_projection.py` inventories both raw-history reads and
turn-output selectors; `examples/agent-workbench/tests/transcript_projection_harness.mjs` executes the
registered production renderer blocks. A new renderer needs an explicit
registry disposition and harness coverage.

The judged layer keeps real-provider Phases 0–4. It judges whether replies fit
the request and whether the product remains understandable. Counts, identities,
suppression, and projection parity belong to the scripted gate. A disagreement
found during a judged run remains an Abort/RCA; never normalize it away.

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
