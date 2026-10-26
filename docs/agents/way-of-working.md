# Way of working: Linear + the repo

Use [issue-tracker.md](issue-tracker.md) for Linear commands,
[ticket-style.md](ticket-style.md) for ticket format, and
[domain.md](domain.md) for vocabulary and ADRs. The
[test backend matrix](test-backends.md) names current law targets and service
recipes. Build commands and reports are in [hermetic-build.md](hermetic-build.md).
Use [change descriptions](pr-style.md) when preparing review evidence.

## Source authority

Read in this order: **code on main**, then **current kernel and spec
contracts**, then **DECISIONS and ADRs in force**. An unmerged branch describes
proposed behaviour. Report disagreements with both code and document evidence;
resolve them at the owning source without weakening its checks.

| Concern | Owning source |
| --- | --- |
| Implemented behaviour, API and executable laws | Code and tests on main, including the facade's public signatures |
| Kernel meaning, library rules and parked state | [Kernel semantics](../kernel/semantics.md), the rule documents in `docs/kernel/`, and [kernel design](../kernel/design.md) for architecture |
| Host integration and boundary contracts | [Durable hosting](../operations/durable-hosting.md) and the current contract for the affected seam, read against its implementation |
| Architectural choices and rulings | The owning ticket's `DECISIONS.md`, when supplied, and accepted ADRs in `docs/adr/`, for the concerns still in force |
| Vocabulary and validation targets | [CONTEXT.md](../../CONTEXT.md) and [test-backends.md](test-backends.md) |

A superseded ADR must mark its status and link its replacement; it never
governs current work. Follow that replacement. An amendment replaces only the
concerns it names. Use **code mode** for generic kernel execution of cells,
documents, workflows and process bodies; reserve **REPL** for the RLM protocol's
persistent interaction across cells.

## The model in one paragraph

Planning, tracking, and everything in-flight live in **Linear** (team `figments`, project `lash`; issues keyed `FIG-<n>`). The repo holds only **durable, code-facing artifacts**: decisions (`docs/adr/`), vocabulary (root `CONTEXT.md`), and agent-driven runbooks (`runbooks/`). The test for "belongs in the repo": *an agent needs it while touching code, at HEAD, offline.* Anything narrative, exploratory, or transient (research reports, plans, decision debates, status) belongs on a Linear issue, not in a repo file.

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
| Housekeeping / teardown / process chores | **Nowhere durable.** Linear is code-facing only; track chores in the session that owns them |

Keep code-facing reference documentation with its owning subsystem. Put
research, plans and status on the ticket. [ticket-style.md](ticket-style.md)
defines the ticket format.

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
- **Two layers, kept separate.** Scripted deterministic harnesses (`runbooks/rlm-smoke/`, the `just *-e2e` recipes) are gate *evidence*: they boot real infrastructure and assert exact outcomes, and they stay scripts. Browser runbooks are the agent-judged semantic layer on top, gating on what the example app actually renders. A runbook never re-implements a scripted harness, and a harness never asks for judgement.
- **Safe to run wholesale.** Every runbook validates; none mutates beyond its own seeded scenario state. An agent told "run the runbooks" must never destroy anything. Destructive or maintenance procedures are not runbooks; they belong in `docs/` as operational reference.
- **Ship the needed proof.** When a named durability or behavior risk needs live validation beyond the current CI plan, the change ships or updates the targeted runbook that proves it. Otherwise, focused local regression evidence and CI satisfy the validation requirements (see Definition of done).
- **Scope flavors:** per-behavior scenarios (regression-style, e.g. one FIG's fix) and per-subsystem validations tied to the ADR they prove out.

## ADR norms

- **Numbering:** next number = highest existing + 1. Check first: `ls docs/adr/ | sort -V | tail -3`. Every four-digit ADR prefix must be unique; nothing enforces that any more (the docs lint that rejected duplicates went with the doc site in FIG-2364), so the `ls` check above is the only guard. An ADR is cited by full filename when a reference needs the slug. Never mint a duplicate.
- **Shape:** one decision per ADR, filename `NNNN-kebab-case-title.md`, status/context/decision/consequences.
- **Status:** mark a superseded decision and link its replacement. Read only the concerns still in force, following the source authority above.
- **Conflicts:** if your output contradicts an ADR still in force, surface it explicitly ("Contradicts ADR-NNNN … worth reopening because …") rather than silently overriding; see [domain.md](domain.md).

## Facade API surface

The `lash` crate is the host API (ADR 0051). Every type named by a facade signature -- parameters, returns, fields, variants, trait members and supertraits, generic bounds, aliases -- is nameable through `lash::`. Export what hosts genuinely need, in the facade module of the item that names it; narrow the rest (`pub(crate)`, a changed signature, or a hidden cross-crate support module such as `core_internal`, `facade_ops` or `core_support`). `#[doc(hidden)]` is not a resolution, except on explicitly test- or support-only items. `scripts/facade_completeness.py` enforces the rule over the rustdoc JSON (`--document-hidden-items`) of `lash` and every first-party library in its dependency closure. It has no allowlist and no exemption mechanism, and no hand-maintained list of API items of any kind may be added to it.

## Host changes in commit bodies

A commit that changes what hosts write, configure or read includes a
"Hosts that ..." paragraph. Name every removed, renamed or retyped public
item and its replacement, and every opt-in the behavior depends on: build
features, configuration fields, policies and non-default detail levels. State
when no new opt-in is required. For example, hosts that want workflow node
spans set `TraceDetailPolicy` for `TraceSpanKind::Workflow` to
`TraceDetail::Extended`; `standard()` does not enable those spans.

## Lifecycle: states and labels

**States are the lifecycle position. Labels carry only what a state can't express: who picks the ticket up.** Keep the two orthogonal; don't say the same thing twice in both.

The workflow states, in order:

- **Triage.** Just arrived, not yet assessed. The inbox. (The state *is* the "needs triage" signal; there is no `needs-triage` label, don't add one.)
- **Backlog.** Accepted as real work, but not now.
- **Todo.** Specified and ready to pick up. This is where dispatch matters (labels below).
- **In Progress.** Claimed and being worked; claiming = assigning.
- **In Review.** The implementation is under review.
- **Done.** Merged and validated per the Definition of Done (below). **Canceled** / **Duplicate**: terminal, not doing it. (`Canceled` covers "won't fix"; there is no `wontfix` label.)

Two dispatch labels, and only these two, live on **Todo** tickets; they say who takes the ticket, which no state can:

- **`ready-for-agent`:** *fully specified*. Success condition stated, no open decisions, an AFK agent can take it without asking anything (its Agent spec is filled in; see [ticket-style.md](ticket-style.md)). If specifying it would take a conversation, it isn't ready; send it back to Triage.
- **`ready-for-human`:** work needing judgment or access an agent doesn't have.

Agents may self-serve from the `ready-for-agent` queue; claiming moves it to In Progress and assigns it.

## Live debugging protocol: evidence first

When diagnosing live behavior (a stall, a wrong decision, a failed run, a lost effect), the first move is the recorded evidence, not an experiment:

1. **Read the trace or structured log for the operation in question** before designing any wait-and-observe or perturbation experiment (the `lash-trace` crate docs cover what Lash emits and how to export it). Most questions of the form "what did it read and why did it decide that" must be answerable from the recorded evidence.
2. **An unanswerable trace is itself a finding.** If the span for a decision exists but lacks the inputs that produced it, or doesn't exist at all, report that instrumentation gap as a first-class defect alongside whatever you eventually diagnose, then fall back to rows and logs.
3. **Experiments are the last resort**, for questions traces structurally cannot answer (timing races, provider-side behavior). Budget them: a multi-cycle observation loop is hours of wall clock that one well-attributed span replaces.

**Decision seams ship decision evidence.** Any gate, claim, lease check, admission check, or adjudicator that can deny, park, cancel, or reroute must emit its full decision basis on the span or structured log: the inputs, the consulted state and its freshness, the thresholds, and the outcome. Reviewers check this like they check tests; "the code denies correctly but you can't see why from a trace" does not pass review.

## Definition of done

[AGENTS.md](../../AGENTS.md) defines implementation proof and landing through
`scripts/land`. State the expected proof on the ticket and report exactly what
ran. A focused run does not establish full-suite coverage; a skipped service
or accepted CI policy skip does not prove the skipped behaviour.

Contract-asserting gates, including schema, boundary, conformance, simulation
and version gates, need a red-side mutation witness when changed. Record the
mutation, its observed failure and why it failed. Formatting and style checks
are outside this rule. Fix deterministic contract failures rather than bypassing
them. Broader diagnostics such as `just push-gate` and confidence lanes remain
opt-in for a named risk, release work or an explicit request.

### Which tests get admitted

A test earns its place only by catching a real bug that no other surviving
test catches. Coverage counts execution; only an independent oracle counts as
checking. Before adding a test, answer four questions in your head, and put the
first answer in the test's name or doc comment:

1. **Which real bug does it catch?** A bug a competent engineer or agent would
   plausibly write here: a missed variant, an off-by-one at this boundary, a
   reordered durable write, a resume that re-enters fresh, a register op that
   charges differently from the semantic reference, a dropped stop point. Or a
   bug that was actually found: the test fails once on the unfixed code.
   "Negate the condition" is not a real bug.
2. **Is it the only test that catches it?** Search the conformance corpus, the
   register-versus-reference differential, `lash-sim`, `lash-regress` and the
   store law runners. If one of them already fails on that bug, add your input
   to it (a corpus case, a seed, a matrix row) instead of writing a new test.
3. **Where does the expected value come from?** A spec, a hand-derived value, a
   reference implementation or an invariant is an oracle. A value computed by
   the code under test or a helper it shares, or pasted from a run (census
   counts, byte totals, digests), is a mirror.
4. **What breaks it that is not a defect?** If a behaviour-preserving refactor
   fails it, it is a change detector.

Admitted:

- a law that pins a named rule with an independent oracle;
- a bug law that failed on the unfixed code;
- hand-picked examples that press a boundary or separate the bug: 129 eligible
  rows for a 128-row page, unequal writes for an isolation claim, a retained
  control beside a reclaimed row;
- a short, deterministic expect transcript a reviewer actually reads;
- a new case added to an existing corpus, differential or property.

Rejected in review (and deleted by the test audit, FIG-7383):

- a wrapper that re-runs a corpus case or law that already runs;
- a per-op or per-change paired law that repeats what the reference
  differential already compares;
- an expected value that comes from the same code, conversion or helper, or a
  number re-pinned from a run;
- a count, presence check or "does not throw" standing in for behaviour
  (`is_some()`, `len() > 200`, `count == 0`);
- internal shape: printed lowering text, private counters, call order, exact
  error prose, current default bounds;
- a title that promises more than the assertions check: a "differential" that
  discards the result, "linear" with no bound, "race" with no unsafe interval;
- a fixture that cannot separate the bug, such as equal writes offered as
  proof of isolation;
- constructors, defaults, getters, `Display`, derives and framework behaviour;
- performance asserted through printed shape (use the lash-perf compare
  driver).

When a change removes machinery, delete its tests and any test module no
target compiles in the same change; rename surviving laws whose names still
say the old machinery. Port a property only when no current law covers it.

Write tests well:

- Witness the state before judging the result; for example, submit 129 eligible rows to exercise a 128-row page boundary.
- Derive accounting and boundary expectations independently from nonempty typed facts; for example, assert one unknown logical call from two lost attempts rather than comparing two uses of the usage helper.
- Give every reclamation example a retained control and distinguish immediate logical expiry from later physical drain; for example, retain a pinned neighbor while an expired row reads Released before its bytes are collected.
- Report the semantic work a service or coverage gate reached; for example, count an executed nonempty model-call read rather than source-text presence, a skipped service, missing credentials or an empty selection.

### Expect tests versus conformance assertions

Use an inline expect test when the review artifact is a short, deterministic behavior transcript and a changed ordering or rendered state should be judged as one coherent diff. Keep conformance suites assertion-based: they prove backend-independent invariants across implementations, where pinning one example interleaving would narrow the contract instead of strengthening it. Never bless an expect diff until its durable-write lines still distinguish the defect the test is meant to catch.

## Authoring rules for concurrent code

Most defects the FIG-6840 audit found share five shapes. Each rule below prevents one; reviewers check them like tests.

1. **Ownership at the claim.** Create the cleanup or invalidation owner at the moment a resource, queue position, reservation, frame or view lifetime is claimed, before the first `.await` or external callback, and transfer that same obligation on success. A future can be dropped at any point it returned `Pending`, so a dropped future must release or hand on what it holds in every state. Test it with the cancellation sweep, `lash_core_ids::cancel_sweep::sweep_drops` (re-exported as `lash_core::cancel_sweep`): it drops a fresh scenario's future after 0, 1, 2, … polls until one run completes and checks your invariant after every cut (the next acquirer proceeds, no reserved, held or sealed state remains, the prior frame is installed). Each sweep law names the site and the abandonment it guards against.
2. **Check and mutation under one owner.** Make the state predicate and its transition one operation under the same synchronization owner. Do expensive sizing, callbacks and awaits outside that critical section, then reclassify under it.
3. **Identity, not position.** Carry an ID with its value and update by that ID. A vector index, FIFO order, a "most recent with this name" search or a sentinel value is never an identity.
4. **Pass facts intact.** The producer of a validated or ordered fact passes it intact to its consumer; normalize only at the boundary that needs the simpler form, never flatten and rebuild.
5. **One inventory owner.** Author a membership list or discriminant once, at its source owner, and project it; consumers own policy, never a second hand-maintained inventory.

## Team and session norms

- Read `CONTEXT.md` and area-relevant ADRs before exploring (see [domain.md](domain.md)).
- Trunk-based: branch from fresh `origin/main` in your own Kiln fork and land
  committed work with `scripts/land`, as described in AGENTS.md. Release
  dispatch remains maintainer-owned; never tag or publish by hand.
- **Nothing load-bearing in private agent memory.** Session memory is a personal cache. Anything a teammate (or their agent) would need must land on Linear or in the repo.
- **Outward text gets a human go-ahead.** Agents draft freely, but PR comments, review replies, and comments on tickets that teammates will read are posted only with the executing human's per-item approval.
