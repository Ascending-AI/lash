# Way of working: Linear + the repo

The norms file for how work in this repo is planned, tracked, decided, validated, and documented. Companions in this directory: [issue-tracker.md](issue-tracker.md) (Linear mechanics), [ticket-style.md](ticket-style.md) (how a ticket reads: altitude, template, prose bar), [pr-style.md](pr-style.md) (how a PR reads: the "how" plus validation proof), [triage-labels.md](triage-labels.md) (triage-role label vocabulary), [domain.md](domain.md) (how to consume the glossary and ADRs).

These files are repo-internal process guidance. They are not part of the published documentation site; the Pages build excludes this directory.

## The model in one paragraph

Planning, tracking, and everything in-flight live in **Linear** (team `figments`, project `lash`; issues keyed `FIG-<n>`). The repo holds only **durable, code-facing artifacts**: decisions (`docs/adr/`), vocabulary (root `CONTEXT.md`), agent-driven runbooks (`runbooks/`), and the published reference site (`docs/`). The test for "belongs in the repo": *an agent needs it while touching code, at HEAD, offline.* Anything narrative, exploratory, or transient (research reports, plans, decision debates, status) belongs on a Linear issue, not in a repo file.

## Routing: where each artifact goes

| You are producing… | It goes… |
| --- | --- |
| A well-understood work item (bug, feature, follow-up) | A plain FIG issue in the `lash` Linear project |
| A big or foggy effort that needs decisions before building | A **parent FIG issue** holding the destination plus a one-line-per-child index, with a child ticket per decision or slice ([ticket-style.md](ticket-style.md)) |
| A research report or an RCA write-up | A **comment on the Linear ticket** that asked for it. Never a repo file |
| An implementation plan or design spec | The parent ticket and its children *are* the plan. A locked design that must outlive them → ADR. There is no `docs/plans/` directory; don't create one |
| A decision with lasting architectural weight | An ADR in `docs/adr/` (see ADR norms) |
| A validation procedure for new or changed live behavior | `runbooks/<scenario>/runbook.md` (see Runbook norms) |
| A new or sharpened domain term | Root `CONTEXT.md` glossary (honor its `_Avoid_` lines). One glossary, no per-crate shadow glossaries |
| Reference documentation for people using Lash | A page on the published docs site: authored per `docs/STYLEGUIDE.md`, registered in `docs/docs.js`, gated by `python3 scripts/lint_docs.py` |
| Housekeeping / teardown / process chores | **Nowhere durable.** Linear is code-facing only; track chores in the session that owns them |

`docs/` is a published website (lash.run), not a scratch directory. Anything you add there is public, must fit the site's structure, and must keep `scripts/lint_docs.py` green.

## How a ticket reads

Routing (above) decides *what goes where*. [ticket-style.md](ticket-style.md) decides *how the ticket reads once you're writing one*. The contract in one line: **a ticket says what, why, and how you'll know it's done, never how to build it.** Implementation detail (file:line, SHAs, RCA, builder code) rides in the PR, the code, an ADR, or a linked report; the ticket links it, never inlines it. Every ticket takes the fixed top (a plain-English **TL;DR** sentence, then `## Why`, then `## Done when`), and every non-trivial ticket hangs off an orienting parent that holds the destination in ≤5 sentences plus a one-line-per-child index. Draft, then run the prose bar before filing. Full template, title rules, and checklist live in [ticket-style.md](ticket-style.md).

The altitude the ticket leaves out lands in the PR, not lost: the ticket says *what* and *why*, the **PR says how and proves it works** (the change's shape, the reasoning, and the validation evidence tied to the Definition of Done). See [pr-style.md](pr-style.md).

## Parent-ticket norms

- **The parent is the index, tickets hold the detail.** The full record of a decision lives in one place: the resolving ticket's comment. Its *conclusion* is echoed as a one-line gist to the parent's index, and (per [ticket-style.md](ticket-style.md)) into the resolving ticket's own body, so a reader (or an agent calling `get_issue` without reading the thread) sees the outcome without digging. Gist echoes, full record doesn't move.
- **Refer by name, never bare key.** `FIG-123` alone is illegible; the key rides inside the linked title.
- **Tickets needing a human name their human.** A decision ticket is only resolvable with the person who has the authority to decide it; say who that is in the ticket. An agent never stands in for the human's side.
- **Graduate durable decisions.** When an effort resolves something with lasting architectural weight, write the ADR as part of resolving the ticket (or as the effort's terminal step). The tracker is an append-only log; the repo is current state. Anything still only-in-a-ticket after its parent closes risks re-litigation.
- **Close the loop.** When the destination is reached: mark remaining children Done/Canceled (no orphaned open tickets), mark the parent Done, and move it out of the Triage state; completed work does not sit in Triage.
- **One live parent per effort.** Before opening a new one, search for an existing parent covering the effort and extend it instead.

## Runbook norms

A **runbook** is an **agent-driven test scenario**: QA performed by an agent against a live example app, with assertable gates staged as **do → expect**, following the shared rules in `runbooks/RULES.md` (poll-don't-sleep, assert the rendered output not just the status, probe values that can't pass by coincidence).

- **Location:** `runbooks/<scenario>/runbook.md`, one scenario per directory. `runbooks/RULES.md` holds every shared rule; a runbook adds only its scenario-specific purpose, phases, and scorecard.
- **Two layers, kept separate.** Scripted deterministic harnesses (`runbooks/restate-postgres-workers/`, the `just *-e2e` recipes) are gate *evidence*: they boot real infrastructure and assert exact outcomes, and they stay scripts. Browser runbooks are the agent-judged semantic layer on top, gating on what the example app actually renders. A runbook never re-implements a scripted harness, and a harness never asks for judgement.
- **Safe to run wholesale.** Every runbook validates; none mutates beyond its own seeded scenario state. An agent told "run the runbooks" must never destroy anything. Destructive or maintenance procedures are not runbooks; they belong in `docs/` as operational reference.
- **Ship with the change.** A PR that creates or changes live behavior ships or updates the runbook that proves it. Merged is not done; live-validated is (see Definition of done).
- **Scope flavors:** per-behavior scenarios (regression-style, e.g. one FIG's fix) and per-subsystem validations tied to the ADR they prove out.

## ADR norms

- **Numbering:** next number = highest existing + 1. Check first: `ls docs/adr/ | sort -V | tail -3`. Duplicate numbers have happened (two each of 0034/0036/0037/0038; grandfathered, do not renumber, cite them by full filename); never mint a duplicate again.
- **Shape:** one decision per ADR, filename `NNNN-kebab-case-title.md`, status/context/decision/consequences.
- **Conflicts:** if your output contradicts an ADR, surface it explicitly ("Contradicts ADR-NNNN … worth reopening because …") rather than silently overriding; see [domain.md](domain.md).

## Lifecycle: states and labels

**States are the lifecycle position. Labels carry only what a state can't express: who picks the ticket up.** Keep the two orthogonal; don't say the same thing twice in both.

The workflow states, in order:

- **Triage.** Just arrived, not yet assessed. The inbox. (The state *is* the "needs triage" signal; there is no `needs-triage` label, don't add one.)
- **Backlog.** Accepted as real work, but not now.
- **Todo.** Specified and ready to pick up. This is where dispatch matters (labels below).
- **In Progress.** Claimed and being worked; claiming = assigning.
- **In Review.** A PR is open against it.
- **Done.** Live-validated per the Definition of Done (below). **Canceled** / **Duplicate**: terminal, not doing it. (`Canceled` covers "won't fix"; there is no `wontfix` label.)

Two dispatch labels, and only these two, live on **Todo** tickets; they say who takes the ticket, which no state can:

- **`ready-for-agent`:** *fully specified*. Success condition stated, no open decisions, an AFK agent can take it without asking anything (its Agent spec is filled in; see [ticket-style.md](ticket-style.md)). If specifying it would take a conversation, it isn't ready; send it back to Triage.
- **`ready-for-human`:** work needing judgment or access an agent doesn't have.

Agents may self-serve from the `ready-for-agent` queue; claiming moves it to In Progress and assigns it.

## Live debugging protocol: evidence first

When diagnosing live behavior (a stall, a wrong decision, a failed run, a lost effect), the first move is the recorded evidence, not an experiment:

1. **Read the trace or structured log for the operation in question** before designing any wait-and-observe or perturbation experiment (`docs/tracing.html` covers what Lash emits and how to export it). Most questions of the form "what did it read and why did it decide that" must be answerable from the recorded evidence.
2. **An unanswerable trace is itself a finding.** If the span for a decision exists but lacks the inputs that produced it, or doesn't exist at all, report that instrumentation gap as a first-class defect alongside whatever you eventually diagnose, then fall back to rows and logs.
3. **Experiments are the last resort**, for questions traces structurally cannot answer (timing races, provider-side behavior). Budget them: a multi-cycle observation loop is hours of wall clock that one well-attributed span replaces.

**Decision seams ship decision evidence.** Any gate, claim, lease check, admission check, or adjudicator that can deny, park, cancel, or reroute must emit its full decision basis on the span or structured log: the inputs, the consulted state and its freshness, the thresholds, and the outcome. Reviewers check this like they check tests; "the code denies correctly but you can't see why from a trace" does not pass review.

## Definition of done

State the expected proof on the ticket. Defaults when unstated:

- **Behavior changes:** live-validated. The relevant runbook (new or existing) passes. Merged-with-green-CI is necessary, not sufficient. Changes to turn execution in `lash-core` or its `lash-restate` adapter additionally run both durable geometries locally (`just agent-workbench-restate-e2e` and `just restate-postgres-workers-e2e`), per `CONTRIBUTING.md`.
- **Mechanical changes** (renames, link sweeps, codegen, doc moves): merged, with the stated verification in the PR.
- **Decisions:** the ADR is merged and the resolving ticket links it.
- **Contract-asserting gates** (schema and drift gates, boundary gates, conformance suites and laws, simulation oracles, coverage and version gates): changes that create or modify one ship with a red-side mutation proof recorded in the PR—the mutation applied, the observed failure, and confirmation that it failed for the stated reason. Formatting and style checks are out of scope. A gate that cannot fail is indistinguishable from no gate; green is what everyone expects to see.

Gate merges on the local battery (`just push-gate`, plus the confidence-gate lane the change warrants) and review; CI is the backstop, not the first signal. Deterministic failure classes (docs lint, conformance, contract drift) must be fixed, never bypassed.

Heavy gates are serial within a lane and capped across the box: build width comes from the environment the checkout was prepared with (`CARGO_BUILD_JOBS`, `NEXTEST_TEST_THREADS`), and the build-heavy legs of `push-gate.sh` run through the `heavy-slot` semaphore when the machine provides it, so concurrent lanes queue instead of thrashing. Both are feature-detected and inert on CI. Do not override either without a measured need; the mechanics are in `CONTRIBUTING.md` under "Concurrent local gates".

A battery may consult `python3 scripts/gate_scope.py --base origin/main` to skip gate families no touched path can reach — a prose-only change does not need the compile battery. The classifier only ever skips what it can prove is unaffected: a shared input (manifests, lockfile, toolchain, `scripts/`, `.github/`), an unrecognised path, an empty path set, or its own failure runs everything. Its decision line must be printed verbatim into the gate log next to the gate table, so a reviewer can audit every skip rather than take it on trust; a battery result reported without that line is reported as if nothing was skipped.

### Expect tests versus conformance assertions

Use an inline expect test when the review artifact is a short, deterministic behavior transcript and a changed ordering or rendered state should be judged as one coherent diff. Keep conformance suites assertion-based: they prove backend-independent invariants across implementations, where pinning one example interleaving would narrow the contract instead of strengthening it. Never bless an expect diff until its durable-write lines still distinguish the defect the test is meant to catch.
When those durable-semantics lines change, follow the named transcript-justification rule in [pr-style.md](pr-style.md).

## Team and session norms

- Read `CONTEXT.md` and area-relevant ADRs before exploring (see [domain.md](domain.md)).
- Trunk-based: `main` is the only long-lived branch, branch from fresh `origin/main`, short-lived branches, merge by PR. Never push product changes straight to `main`, and never tag or publish a release by hand; see `CONTRIBUTING.md` and `docs/PUBLISHING.md`.
- PRs are not a request surface; work items live in Linear, and GitHub is for code review only.
- **Nothing load-bearing in private agent memory.** Session memory is a personal cache. Anything a teammate (or their agent) would need must land on Linear or in the repo.
- **Outward text gets a human go-ahead.** Agents draft freely, but PR comments, review replies, and comments on tickets that teammates will read are posted only with the driving human's per-item approval.
- **Skills:** this workflow assumes the grilling / research / domain-modeling / triage skill set is installed for your agent; the repo-side bindings are the files in `docs/agents/`.
