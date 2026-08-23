# Domain docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase. This repo is **single-context**: one root `CONTEXT.md` glossary + one `docs/adr/`.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root: the ubiquitous-language glossary (Host Application, Execution Mode, Runtime Scenario, Trigger Occurrence, Pending Turn Input, Queued Work, and the rest).
- **`docs/adr/`**: read the ADRs that touch the area you're about to work in.

For a narrative map of the runtime rather than its vocabulary, the architecture chapters on the published docs site (`docs/architecture/`) orient faster than reading crates cold.

If any of these files don't exist, **proceed silently**. Don't flag their absence; don't suggest creating them upfront. The `/domain-modeling` skill creates them lazily when terms or decisions actually get resolved.

## File structure

Single-context (this repo):

```
/
├── CONTEXT.md
├── docs/adr/
│   ├── 0026-model-capability-is-host-supplied-data.md
│   ├── 0045-services-are-stateless-substrates-own-continuation.md
│   └── …
└── crates/ · examples/ · runbooks/
```

## Use the glossary's vocabulary

When your output names a domain concept (an issue title, a refactor proposal, a hypothesis, a test name), use the term as defined in `CONTEXT.md`, and honor its `_Avoid_` lines. Don't drift to synonyms the glossary explicitly avoids (for example, don't call a Host Application a "reference host", and don't call the Deterministic Simulation Harness an "e2e fuzz test").

If the concept you need isn't in the glossary yet, that's a signal: either you're inventing language the project doesn't use (reconsider) or there's a real gap (note it for `/domain-modeling`).

## Flag ADR conflicts

If your output contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0045 (services are stateless, substrates own continuation), but worth reopening because…_

ADR numbers are unique. Cite an ADR by full filename when a reference needs the slug ([way-of-working.md](way-of-working.md) has the numbering rules).
