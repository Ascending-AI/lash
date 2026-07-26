# Writing pull requests

[ticket-style.md](ticket-style.md) keeps tickets high-level; the detail it pushes out lands here. A ticket says *what* and *why*. The **PR says how, and proves it works**. This is where file:line reasoning, the shape of the change, and the validation evidence belong.

PRs are a code-review surface, not a request surface; work items live in Linear ([way-of-working.md](way-of-working.md)). A PR exists to get a change reviewed and merged, so write it for the reviewer.

**Why the *why* lives in the ticket, not here: a deliberate departure.** Git-native convention (Google CL descriptions, Conventional Commits) anchors the *why* in the commit message. We anchor it in the ticket instead, because a ticket must be plannable, by a human or an AFK agent, before any code or diff exists. One consequence to respect: the PR body is a review artifact, not durable history. Anything that must outlive this diff belongs in the ticket, an ADR, the code, or — for user-facing changes — the `Release-Notes:` commit section below.

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
alternative would still matter independent of this diff (a datastore, protocol,
or cross-cutting-pattern choice), promote it to an ADR and link it; only
diff-local trade-offs stay here.

## Validation
How you know it works, matched to the Definition of Done: repeatable steps a
reviewer could rerun, i.e. which runbook passed, the command, what you checked
live. Not "tested locally". "Merged with green CI" is necessary, not
sufficient, for behavior changes.

## Risk / not included   (optional)
Blast radius, anything deliberately out of scope, follow-ups filed as tickets.
```

## Release notes are required for user-facing changes

lash ships curated release notes, and they are collected **from commit bodies**, not from PR bodies. Any commit that should contribute user-facing notes carries a `Release-Notes:` section — everything after the marker line, written as Markdown:

```text
Add durable suspension to processes

Implementation details for reviewers...

Release-Notes:
- Processes now suspend durably while waiting on signals or timers.
- Signals are named and typed; the unnamed `wait_signal()` is removed.
```

The release workflow runs `scripts/release_notes.py collect --require` before tagging: **if no commit in the release range carries a section, the release stops without publishing.** So a user-facing change whose notes live only in the PR body blocks the next release. Full mechanics in [`docs/PUBLISHING.md`](../PUBLISHING.md).

Write them for a user of the library, not a reviewer of the diff: what changed for them, what breaks, what they must now do differently.

## Rules

- **The first line is load-bearing.** It becomes the commit title in `git log`, so write it imperative, one sentence, standalone: `Add cursor pagination to chat thread list`, never `Fixes`, `WIP`, or `Phase 1`.
- **Lead with the summary.** A reviewer reads the first paragraph and knows what the PR does and why. Depth follows; it doesn't open the PR.
- **Link the ticket, don't restate it.** One or two orienting sentences, then the link. The PR carries the *how*; the ticket carries the *what/why*. Don't copy the ticket body in.
- **Prove it, per the Definition of Done.** Behavior changes are live-validated (the relevant runbook passes); mechanical changes state the verification you ran ([way-of-working.md](way-of-working.md)). Say what you actually did, including what you skipped.
- **Match the diff.** The PR body describes what the diff does. No aspirational claims for code that isn't there, no stale description after a force-push.
- **Follow-ups are tickets, not TODOs.** Work discovered but out of scope gets a FIG issue and a link, not a buried comment.
- **The ticket's prose bar applies here too:** lede first, cut filler, concrete over abstract ([ticket-style.md](ticket-style.md)). A reviewer skims, and a rambling PR body costs review time.

## Mechanics (defined elsewhere)

- **Commits.** No AI-assistant attribution or co-author trailers; see the global agent rules. Never write the literal `[skip ci]` token in a commit body — it suppresses the push's workflows; write `skip-ci` when referring to it.
- **Branching.** Trunk-based: branch from fresh `origin/main`, short-lived branches, merge by PR. Never push product changes directly to `main`; never tag or publish a release manually ([`CONTRIBUTING.md`](../../CONTRIBUTING.md)).
- **Stacked PRs.** For dependent chains, use the native stack flow (global agent rules). Stacked PRs are enabled on this repo.
