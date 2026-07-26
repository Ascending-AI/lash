# Way of working: Wayfinder + Linear

The norms file for how work in this repo is planned, tracked, decided, validated, and documented. Companions in this directory: [issue-tracker.md](issue-tracker.md) (Linear mechanics, including the "Wayfinding operations" section), [ticket-style.md](ticket-style.md) (how a ticket reads: altitude, template, prose bar), [pr-style.md](pr-style.md) (how a PR reads: the "how" plus validation proof, and the `Release-Notes:` requirement), [triage-labels.md](triage-labels.md) (triage-role label vocabulary), [domain.md](domain.md) (how to consume the glossary and ADRs).

## The model in one paragraph

Planning, tracking, and everything in-flight live in **Linear** (team `figments`, project `lash`). The repo holds only **durable, code-facing artifacts**: decisions (ADRs), vocabulary (`CONTEXT.md`), agent-executed runbooks (`runbooks/`), and reference docs. The test for "belongs in the repo": *an agent needs it while touching code, at HEAD, offline.* Anything narrative, exploratory, or transient (research reports, plans, decision debates, status) belongs on a Linear issue, not in a repo file.

## Routing: where each artifact goes

| You are producing… | It goes… |
| --- | --- |
| A well-understood work item (bug, feature, follow-up) | A plain FIG issue in the `lash` project |
| A big or foggy effort that needs decisions before building | A **wayfinder map**: one Linear issue labelled `wayfinder:map`, child decision tickets labelled `wayfinder:research` / `wayfinder:prototype` / `wayfinder:grilling` / `wayfinder:task` (via the `/wayfinder` skill) |
| A research report | A **comment on the Linear ticket** that asked for it. Never a repo file |
| An implementation plan or design spec | The wayfinder map and its tickets *are* the plan. A locked design that must outlive its map → ADR. Never a plans/specs file in the repo |
| A decision with lasting architectural weight | An ADR in `docs/adr/` (see ADR norms) |
| A validation procedure for new or changed live behavior | `runbooks/<scenario>/runbook.md` (see Runbook norms) |
| A new or sharpened domain term | Root `CONTEXT.md` glossary (via `/domain-modeling`; honor `_Avoid_` lines). One glossary, no per-subsystem shadow glossaries |
| Reference documentation (harness mechanics, ops setup, conventions) | `docs/<topic>.md` (things an agent *reads*, never executes) |
| User-facing notes for a shipped change | A `Release-Notes:` section in the **commit body** ([pr-style.md](pr-style.md)) |
| Housekeeping / teardown / process chores | **Nowhere durable.** Linear is code-facing only; track chores in the session that owns them |

## How a ticket reads

Routing (above) decides *what goes where*. [ticket-style.md](ticket-style.md) decides *how the ticket reads once you're writing one*. The contract in one line: **a ticket says what, why, and how you'll know it's done, never how to build it.** Implementation detail (file:line, SHAs, RCA, builder code) rides in the PR, code, an ADR, or a linked report; the ticket links it, never inlines it. Every ticket takes the fixed top (a plain-English **TL;DR** sentence, then `## Why`, then `## Done when`), and every non-trivial ticket hangs off an orienting parent (a wayfinder map or a plain parent) that holds the destination in ≤5 sentences plus a one-line-per-child index. Draft, then run the prose bar before filing. Full template, title rules, and checklist live in [ticket-style.md](ticket-style.md).

The altitude the ticket leaves out lands in the PR, not lost: the ticket says *what* and *why*, the **PR says how and proves it works**. See [pr-style.md](pr-style.md).

## Wayfinder lifecycle norms

The `/wayfinder` skill and the "Wayfinding operations" section of [issue-tracker.md](issue-tracker.md) define the mechanics (map body, claiming, blocking, frontier). Norms on top of that:

- **The map is the index, tickets hold the detail.** The full record of a decision lives in one place: the resolving ticket's comment. Its *conclusion* is echoed as a one-line gist to the map's Decisions-so-far, and (per [ticket-style.md](ticket-style.md)) into the resolving ticket's own body, so a reader (or an agent calling `get_issue` without reading the thread) sees the outcome without digging. Gist echoes, full record doesn't move.
- **Refer by name, never bare key.** `FIG-123` alone is illegible; the key rides inside the linked title.
- **HITL tickets name their human.** A `wayfinder:grilling` or `wayfinder:prototype` ticket is only resolvable with the person who has the authority to decide it; say who that is in the ticket. An agent never stands in for the human's side.
- **Graduate durable decisions.** When a map resolves something with lasting architectural weight, write the ADR as part of resolving the ticket (or as the map's terminal step). The tracker is an append-only log; the repo is current state. Anything still only-in-a-ticket after its map closes risks re-litigation.
- **Close the loop.** When the destination is reached: mark remaining children Done/Canceled (no orphaned open tickets), mark the map Done, and move it out of the Triage state; completed work does not sit in Triage.
- **One live map per effort.** Before charting a new map, search `wayfinder:map` for an existing one covering the effort and extend it instead.

## Runbook norms

`runbooks/` has **two layers**, and [`runbooks/RULES.md`](../../runbooks/RULES.md) is the shared contract both read before running:

- **Scripted deterministic harnesses** (e.g. `runbooks/restate-postgres-workers/`) are gate **evidence**: they boot real infrastructure and assert exact outcomes. They stay scripts and never ask for judgement.
- **Browser runbooks** are the **agent-judged semantic layer**: an agent drives the example apps through browser automation and judges what the surface actually renders.

Keep the layers separate — a runbook never re-implements a scripted harness, and a scripted harness never asks for judgement.

- **Location:** `runbooks/<scenario>/runbook.md`, one scenario per directory.
- **Safe to run wholesale.** Every runbook validates; none mutates beyond its own seeded scenario state. An agent told "run the runbooks" must never destroy anything.
- **Ship with the change.** A PR that creates or changes live behavior ships or updates the runbook that proves it. Merged is not done; live-validated is.
- CLI operator runbooks live in the lash-cli repository, not here.

## ADR norms

- **Numbering:** next number = highest existing + 1. Check first: `ls docs/adr/ | sort -V | tail -3`. Duplicate numbers have happened (two each of 0034/0036/0037/0038; grandfathered, do not renumber, cite them by full filename); never mint a duplicate again.
- **Shape:** one decision per ADR, filename `NNNN-kebab-case-title.md`, status/context/decision/consequences.
- **Conflicts:** if your output contradicts an ADR, surface it explicitly ("Contradicts ADR-NNNN … worth reopening because …") rather than silently overriding; see [domain.md](domain.md). An ADR that is *factually wrong about the code* gets corrected, not worked around.

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

- **`ready-for-agent`:** *fully specified*. Success condition stated, no open decisions, an AFK agent can take it without asking anything (its Agent spec is filled in; see [ticket-style.md](ticket-style.md)). If specifying it would take a conversation, it isn't ready; send it back to Triage or make it a wayfinder ticket.
- **`ready-for-human`:** work needing judgment or access an agent doesn't have.

Agents may self-serve from the `ready-for-agent` queue; claiming moves it to In Progress and assigns it, same as wayfinder tickets.

## Definition of done

State the expected proof on the ticket. Defaults when unstated:

- **Behavior changes:** live-validated. The relevant runbook (new or existing) passes. Merged-with-green-CI is necessary, not sufficient.
- **Mechanical changes** (renames, link sweeps, codegen, doc moves): merged, with the stated verification in the PR.
- **Decisions:** the ADR is merged and the resolving ticket links it.
- **User-facing changes:** a `Release-Notes:` section in the commit body, or the next release cannot publish ([pr-style.md](pr-style.md)).

Gate merges on the local battery + review; CI is a sanity glance. Run `cargo check --workspace --all-targets` — never bare `check`, which lets test-cfg struct drift hide — plus the workspace suite. Replicate CI-only gates locally (clippy with `-D warnings`) rather than paying a round trip per surprise. Known-flaky reds may be bypassed; deterministic failure classes must be fixed.

## Team and session norms

- Read `CONTEXT.md` and area-relevant ADRs before exploring (see [domain.md](domain.md)).
- Trunk-based: `main` is the sole long-lived branch, changes ship by short-lived PR, no staging branch or staging release channel. `CONTRIBUTING.md` has the full workflow. PRs are not a request surface; work items live in Linear, and GitHub is for code review only.
- **Nothing load-bearing in private agent memory.** Session memory is a personal cache. Anything a teammate (or their agent) would need must land on Linear or in the repo.
- **Outward text gets a human go-ahead.** Agents draft freely, but PR comments, review replies, and comments on shared maps that teammates will read are posted only with the driving human's per-item approval.
- **Skills:** this workflow assumes the wayfinder / grilling / research / domain-modeling / triage skill set (mattpocock/skills) is installed for your agent; the repo-side bindings are the files in `docs/agents/`.
