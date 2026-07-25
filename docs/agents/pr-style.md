# Writing pull requests

[ticket-style.md](ticket-style.md) keeps tickets high-level; the detail it pushes out lands here. A ticket says *what* and *why*. The **PR says how, and proves it works**. This is where file:line reasoning, the shape of the change, and the validation evidence belong.

PRs are a code-review surface, not a request surface; work items live in Linear ([way-of-working.md](way-of-working.md)). A PR exists to get a change reviewed and merged, so write it for the reviewer.

**Why the *why* lives in the ticket, not here: a deliberate departure.** Git-native convention (Google CL descriptions, Conventional Commits) anchors the *why* in the commit message. We anchor it in the ticket instead, because a ticket must be plannable, by a human or an AFK agent, before any code or diff exists. One consequence to respect: the squash commit body is durable history in this repo and feeds the release notes (below), while review scaffolding in the PR body is not. Anything that must outlive this diff belongs in the ticket, an ADR, the code, or the commit body, not only in review comments.

## What a PR carries

```markdown
Title: imperative, one sentence, readable without opening the ticket.

## Why
Link the FIG ticket, and restate the goal in one or two sentences so the
reviewer needn't open it. The ticket has the full context; this orients.

## What changed
The shape of the change and why this shape: the reasoning a reviewer needs
to judge it. Name the key files/seams, the trade-offs taken, the alternatives
rejected. This is the detail the ticket deliberately left out. If a rejected
alternative would still matter independent of this diff (a persistence,
protocol, or cross-cutting-pattern choice), promote it to an ADR and link it;
only diff-local trade-offs stay here.

## Validation
How you know it works, matched to the Definition of Done: repeatable steps a
reviewer could rerun, i.e. which runbook passed, which `just` recipe or
confidence-gate lane you ran, what you checked live. Not "tested locally".
"Merged with green CI" is necessary, not sufficient, for behavior changes.

## Risk / not included   (optional)
Blast radius, anything deliberately out of scope, follow-ups filed as tickets.
```

## Rules

- **The first line is load-bearing.** It becomes the squash commit title in `git log`, so write it imperative, one sentence, standalone: `Add cursor pagination to the process registry listing`, never `Fixes`, `WIP`, or `Phase 1`.
- **User-facing changes carry release notes.** The squash commit body must include a `Release-Notes:` section describing the change for Lash's users (breaking changes say `Breaking:` first). The release workflow collects those sections and refuses to publish a range that has none. See `docs/PUBLISHING.md`.
- **Lead with the summary.** A reviewer reads the first paragraph and knows what the PR does and why. Depth follows; it doesn't open the PR.
- **Link the ticket, don't restate it.** One or two orienting sentences, then the link. The PR carries the *how*; the ticket carries the *what/why*. Don't copy the ticket body in.
- **Prove it, per the Definition of Done.** Behavior changes are live-validated (the relevant runbook passes; turn-execution changes run both durable geometries); mechanical changes state the verification you ran ([way-of-working.md](way-of-working.md)). Say what you actually did, including what you skipped.
- **Match the diff.** The PR body describes what the diff does. No aspirational claims for code that isn't there, no stale description after a force-push.
- **Follow-ups are tickets, not TODOs.** Work discovered but out of scope gets a FIG issue and a link, not a buried comment.
- **The ticket's prose bar applies here too:** lede first, cut filler, concrete over abstract ([ticket-style.md](ticket-style.md)). A reviewer skims, and a rambling PR body costs review time.

## Mechanics (defined elsewhere)

- **Commits.** No AI-assistant attribution or co-author trailers; see the root agent rules.
- **Branching and releases.** Short-lived branches off fresh `origin/main`, merged by PR; releases are dispatched manually by a maintainer from a green `main` (`CONTRIBUTING.md`, `docs/PUBLISHING.md`). Never tag or publish by hand.
- **Stacked PRs.** For dependent chains, use the native stack flow (global agent rules).
- **Generated and gated artifacts.** Run `just push-gate` before pushing. Docs changes must keep `python3 scripts/lint_docs.py` green, and embedded Rust snippets are regenerated with `python3 scripts/lint_docs.py --fix-snippets` rather than hand-edited.
