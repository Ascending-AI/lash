# Domain docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase. This repo is **single-context**: one root `CONTEXT.md` glossary + one `docs/adr/`.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root: the ubiquitous-language glossary (Host Application, Execution Mode, Runtime Scenario, Pending Turn Input, Queued Work, and the rest).
- **Current contracts** for the area: code mode uses `docs/kernel/semantics.md`
  and its companion rule documents; host integration uses
  `docs/operations/durable-hosting.md`.
- **`docs/adr/`**: read the ADRs still in force for the area you're about to
  work in; follow replacement links on superseded decisions. Apply the
  [source-authority precedence](way-of-working.md#source-authority) when sources
  disagree.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill creates them lazily when terms or decisions actually get resolved.

## File structure

Single-context (this repo):

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0137-the-host-owns-events-routing-and-scheduling.md
│   ├── 0139-the-lash-vm-is-a-dialect-free-kernel.md
│   └── …
└── crates/ · examples/ · runbooks/
```

## Use the glossary's vocabulary

When your output names a domain concept (an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md`, and honor its `_Avoid_` lines. Don't drift to synonyms the glossary explicitly avoids (for example, don't call a Host Application a "reference host", and don't call the Deterministic Simulation Harness an "e2e fuzz test").

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an ADR still in force, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0137 (the host owns events, routing and scheduling), but worth reopening because…_

ADR numbers are unique. Cite an ADR by full filename when a reference needs the slug ([way-of-working.md](way-of-working.md) has the numbering rules).
