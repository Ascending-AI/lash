# Writing tickets

[way-of-working.md](way-of-working.md) says *where* each artifact goes. This says *how a ticket reads* once you're writing one. It applies to every FIG issue (bugs, features, decisions, PR slices), drafted by a human or an agent.

The goal: a reader gets what a ticket is about from the first sentence, and never has to read an investigation to understand the work. Detail belongs in the PR, the code, an ADR, or a linked report, not above the fold in the ticket (one exception: `ready-for-agent` tickets, below).

## The altitude contract

**A ticket says *what*, *why*, and *how you'll know it's done*. It does not say *how to build it*.**

File:line references, commit SHAs, builder-call code, schema conventions, ADR clause numbers, and RCA transcripts belong in the PR description, the code, an ADR, or a linked scratchpad report, not the ticket body. If a reader needs your investigation to understand the ticket, **link it; don't inline it.**

This is the rule to cite in review. A ticket that opens with `ADR 0045 finding 1, clauses 1+4; reports: foo.md, bar.md` has failed it: the reader hits references before they know what is broken.

**One exception, by design:** a `ready-for-agent` ticket carries the how-to detail an agent needs to start, but tucked in a collapsed Agent spec, not smeared through the body. See [Ready-for-agent tickets](#ready-for-agent-tickets-the-agent-spec).

## One ticket, one outcome

A ticket describes one task with one checkable outcome: a piece of code, a fix, a decision, a document. If it can't be stated as one outcome, it's not a ticket:

- A big or foggy effort → a parent ticket with child tickets ([way-of-working.md](way-of-working.md)).
- A raw problem you haven't scoped → still a ticket, but framed as the problem, not a guessed solution. Let the assignee rewrite it as a task once scoped.
- Housekeeping, teardown, process chores → nowhere durable (Linear is code-facing).

**Escape hatch.** The template and the orienting-parent rule below are for real work items. A trivial or mechanical ticket (a dependency bump, a one-line config change, an obvious revert) skips both: a plain title plus a one-line `## Done when` is enough. Don't make a chore fill out a form; forced templates just raise abandonment.

## The template

Same top on every ticket, so readers learn one scan path. A reader who stops after the TL;DR already knows what the ticket is.

```markdown
**TL;DR:** one plain-English sentence saying what this is and why it matters.

## Why
2–4 sentences. The problem or motivation, concrete, with real stakes.
Bugs: what breaks and for whom. Quote or link the evidence, don't paste the RCA.

## Done when
The observable success condition. Checkable, e.g. "runbook X passes",
"the conformance suite covers the reopen path", "ADR-N is recorded and linked here".

## Out of scope   (required if non-trivial; one line each)
What this deliberately does not do; the boundaries that stop scope creep.
```

Anything deeper (file:line notes, design sketches, the full RCA) goes **below `## Done when`**, in a collapsible block at the very bottom or behind a link, never above it. Linear's collapsible is a `+++` fence (not HTML `<details>`, which Linear prints as raw tags): a line `+++ Summary` opens it, a closing `+++` on its own line ends it, and it starts collapsed.

Notes on the fields:

- **`## Out of scope` is required for any non-trivial ticket** (trivial/mechanical tickets skip it with the rest of the template). Naming the boundaries is how you stop scope creep before it starts; an omitted no-go is where a rabbit hole gets in. `ready-for-agent` tickets state theirs as the mandatory "Do not touch" in the Agent spec.
- **TL;DR is skippable when the title already states the outcome as a full sentence.** Don't say the same thing twice. Keep it when the title is necessarily compressed (a bug title naming the symptom, not the fix).
- **`## Done when` must be checkable, not aspirational:** an observable end state a reader could confirm. For a **behavior change**, name the runbook that proves it (new or existing); that *is* the Definition of Done ([way-of-working.md](way-of-working.md)), and "merged with green CI" is necessary, not sufficient. For a `ready-for-agent` ticket, make it *machine*-checkable: a test name, a command, an assertable behavior the agent can loop against. An agent with no runnable check knows only that it's *plausibly* done.

Variants share the same top:

- **Bug.** Write `## Why` as a narrated incident, not an abstract symptom: *"The worker crashed mid-turn → the session reopened on a second worker → the first attempt's tool result was replayed and charged twice."* Concrete beats "effect replay is unreliable."
- **PR slice** (one deliverable in a numbered arc). Often just TL;DR + `## Done when`. Short because the scope is one PR, not because detail was cut. Keep who-does-it and who-reviews-it out of the ticket; that's execution process, not the work.
- **Decision.** The outcome is an ADR. `## Done when` = "ADR-N merged and linked here."

## Ready-for-agent tickets: the Agent spec

A `ready-for-agent` ticket has a second reader: an autonomous coding agent that executes it unattended, with no human to ask mid-task. The evidence on what such an agent needs is measured, and it cuts against the human skim path. Naming the files, giving the intended approach, and setting hard scope boundaries each raise an agent's success rate; vague scope gets resolved by guessing, sometimes destructively. But padding hurts too. So we keep the two readers apart: the human gets the clean top; the agent gets a spec in a **collapsible `+++` block at the very bottom**, after everything a human skims (or a Linear document attached to the issue, which the agent's integration loads on start). Linear renders the `+++` fence as a toggle that starts collapsed, so a human sees only the summary line and expands it only if they need the detail. The agent reads the whole block.

When you apply `ready-for-agent`, add this as the final section (a `+++` collapsible; Linear prints raw `<details>` tags, so never use HTML):

```markdown
+++ ## Agent spec

- **Files.** The modules/paths in play, and the pattern to follow (name a similar existing call site).
- **Approach.** The intended shape, in a sentence or two. Direction, not line-by-line code.
- **Do not touch.** Files/systems out of bounds. If you hit an undecided fork, stop and comment, don't guess.
- **Done check.** The runnable oracle from `## Done when` (a test/command that must pass).
+++
```

Rules:

- **Minimal is not short.** Give the agent the concrete detail it needs; the failure mode isn't length, it's padding. A precise Agent spec doesn't violate the altitude contract; it honors it by keeping the detail below the fold.
- **"Do not touch" is mandatory here, not optional.** It's the one section that stops an unattended agent from resolving ambiguity the destructive way.
- **Don't route agent-critical detail through the comment thread.** Comments aren't reliably loaded into an agent's context; put what the agent must have in the spec block or an attached document. (This deliberately overrides the `triage` skill's `AGENT-BRIEF` default, which posts the spec as a *comment* and avoids file paths. In this repo the spec is an in-body block and naming files is encouraged; pair each with the pattern to follow, so a path that goes stale is self-correcting.)
- **Size gate.** If understanding the task needs more than ~2–3 files of unfamiliar context, split it before labeling it ready-for-agent; "no open decisions" isn't enough on its own.

## Long-form work: research, RCA, decisions

Some tickets exist *to produce* long-form output: a research question, an investigation, a decision to grill out. The altitude contract still holds. The **body poses the ask** and states what a good answer looks like (`## Done when`); the **output lands as a comment** on the ticket. Research reports and RCA write-ups go in comments, never a repo file, never the ticket body ([way-of-working.md](way-of-working.md) routing).

Two durability rules keep the comment thread from swallowing the decision:

- **Echo the decision back into the body.** Once a comment-borne finding or decision is *accepted*, paste a 1–3 sentence synthesis into the ticket body (a `## Decision` line, or under `## Done when`) before the ticket closes or goes `ready-for-agent`. The full report stays in the comment; the *conclusion* lives where every reader will find it: a human skimming, or an agent calling `get_issue` without reading comments. A decision stranded in a thread is one un-read comment away from being lost.
- **Never cite a non-durable artifact as the sole record.** A session scratchpad path, an unmerged branch, or a chat message doesn't count as linked evidence; they rot or vanish. Only a resolved comment on the ticket, a PR pinned to a commit SHA (not a branch), an ADR, or a repo file is a durable link. (This overrides the `/research` skill's "write findings to a repo file" default; in this repo the durable home for a research finding is a comment on the ticket.)

## Titles

A plain sentence naming the outcome. `Process registry listing is unbounded and needs a cursor` reads at a glance. A 20-word RCA clause like `non-Restate AwaitEvent is process-local; SQLite hosts advertise Durable for a sticky path` is a line out of the investigation, not a title. The key rides inside the linked title; never refer to a ticket by bare `FIG-123`.

## Every non-trivial ticket has an orienting parent

Anything past a single-PR slice hangs off an orienting parent issue. This is the "overarching ticket" that lets a reader orient before diving in.

Don't force it, though. Join a parent when one exists; the first ticket of a new arc *creates* the parent rather than waiting to be filed under one; and a genuinely standalone ticket (a one-off bug, a lone maintenance task) needs no parent at all, a sibling link is enough. The goal is orientation, not a hierarchy every ticket must report to. A parent that never closes because nobody shut its children is worse than no parent.

The parent:

- States the **destination in ≤5 plain sentences**: where we're going and why, no implementation surfaces.
- Carries a **one-line-per-child index**, each line linking its ticket.
- **Orients; it is not a changelog.** What shipped and current status live on the children and their PRs. Don't let the parent's destination turn into a run-on status report.

## Prose bar (run before you file)

Draft, then edit; the first draft is material, not the ticket. Run this pass before filing:

1. **Lede.** Is the ask in sentence one? If a reader stops after it, do they know what's being asked?
2. **Altitude.** Any file:line, SHA, builder code, or ADR clause in the body? Move it out or link it.
3. **Cut filler.** Strip "it's important to note", "arguably", "several" (where a number belongs), "basically", and throat-clearing openers ("Before diving in…"). If a sentence's meaning survives deleting the word, the word was dead.
4. **Kill zombie nouns.** `-tion/-ment/-ance` nouns with `is/are/was` as the main verb hide the actor. "Dependency X blocks the migration", not "completion of the migration was blocked by dependency issues".
5. **Concreteness.** Every abstract claim ("causes reliability issues", "improves performance") has a number, file, error, or example, or it's flagged as a gap to fill before filing.
6. **Bullet audit.** No bullet is a sentence fragment standing in for a real claim, and no list is wall-to-wall `**Bold:** description` (the tell of a padded, un-edited draft). Prose or a real claim, not a template.
7. **Strip the AI tells.** They read as machine-drafted and cost the reader's trust:
   - *Em-dashes.* Don't use the em dash as your default connector. Most become a period, comma, colon, or parentheses. Aim for at most one per paragraph.
   - *Phrasebank words.* leverage, robust, seamless, crucial, pivotal, streamline, delve, harness, foster, underscore, "ever-evolving". Use the plain word.
   - *Antithesis / false reframe.* "not just X, it's Y"; "this isn't a bug fix, it's a redesign". If Y only restates X, drop the frame.
   - *Chat fossils and decoration.* "Great question", "I hope this helps", "Let me break this down", sycophantic openers, decorative emoji on headings or bullets. Delete outright.
8. **Length gate.** Shorter without losing a decision-relevant fact? Omit needless words; don't omit needed ones.
