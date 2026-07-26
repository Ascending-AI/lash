# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase. This repo is **single-context**: one root `CONTEXT.md` glossary + one root `docs/adr/`.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root: the ubiquitous-language glossary (Session, Turn, Node, Effect Host, Work Driver, Process, Trigger, Claim, etc.).
- **`docs/adr/`**: read ADRs that touch the area you're about to work in.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill (reached via `/grill-with-docs` and `/improve-codebase-architecture`) creates them lazily when terms or decisions actually get resolved.

## File structure

Single-context (this repo):

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0012-durable-waits-via-effect-host-engines.md
│   ├── 0029-claims-are-generation-fenced-under-the-session-lease.md
│   ├── 0045-services-are-stateless-substrates-own-continuation.md
│   └── …
├── docs/agents/          ← these norms files
├── crates/ · examples/ · runbooks/
```

## Use the glossary's vocabulary

When your output names a domain concept (an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md`, and honor its `_Avoid_` lines. Don't drift to synonyms the glossary explicitly avoids.

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0045 (Services are stateless, substrates own continuation), but worth reopening because…_

An ADR that is **factually wrong about the code** is a different case from one you disagree with: correct it, and say so in the PR. ADR 0012 is the standing example — it lists a lash-owned effect journal as *rejected*, while both SQL substrates ship exactly that.
