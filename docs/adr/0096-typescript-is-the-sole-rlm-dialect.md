# 0096: TypeScript is the sole RLM dialect; lashlang names the IR and VM

Status: Accepted (FIG-3016, 2026-09-13). Supersedes
[ADR 0061](0061-two-first-class-rlm-dialects-with-full-parity-and-session-pinning.md)
and [ADR 0063](0063-one-rlm-turn-is-prompted-in-one-dialect.md). Amends
[ADR 0037](0037-lashlang-workflows-use-a-code-graph-code-lens.md),
[ADR 0055](0055-lashlang-execution-bounds-span-durable-process-lifetimes.md),
[ADR 0060](0060-the-lashlang-vm-is-a-heap-substrate-with-dialect-lowered-value-semantics.md),
[ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md) and
[ADR 0064](0064-the-typescript-dialect-is-broad-and-every-gap-is-an-explicit-ruling.md).

Amended by FIG-3571 (arc FIG-3570): artifact identity hashes the linked IR the
artifact carries, names included. `ModuleArtifact::ir` is that program,
verbatim; there is no normalized or renamed copy, and `module_ref` hashes its
binder names along with its structure and hidden process arguments. Two
alpha-variant cells are therefore two distinct modules with two refs, each
stored immutably (law L9); the earlier normalizer that made them share a ref is
deleted. The program's `language` (the front end that lowered it) is part of
the identity, so two front ends never share a module ref. The measured cost on
a multi-session corpus was about 1.3% more artifacts and stored bytes.

## Context

ADR 0061 made two dialects first-class, permanently, at full parity, and
accepted a doubled battery as the price. The price turned out to be paid twice
over, in a currency that ADR did not price: every ruling forks.

The evidence is on the tickets of the current window. FIG-2986 rules that
TypeScript triggers take the event as a parameter and, in the same breath, has
to say that Lashlang keeps `trigger.event` — one ruling, two answers, because
parity obliges a Lashlang answer to exist. FIG-2999 deletes `defineProcess`,
`start`, `wake`, `registerTrigger` and the `signals` block, and has to delete
them from two surfaces with two lowerings. FIG-3001 re-authors the codemode
parity examples, the runbook and the Restate replay fixtures for both. None of
those are dialect work; they are runtime and language-design work that a second
surface taxes on the way through.

The tax is not buying much. Since August the `lash-typescript` crate has taken
40 commits and the lashlang surface — lexer, parser, canonical printer — has
taken 12. The work is already in one dialect. So are the models: a model
arrives knowing TypeScript, and ADR 0062 exists precisely because that prior is
the strongest asset the dialect has. Nothing comparable accrues to a bespoke
surface, and every gap ruling under ADR 0064 has to be spent teaching one.

The retirement is unusually cheap on the side that would normally make it
expensive. Artifact identity hashes the canonical IR, not canonical source. No
process id, effect id, bytecode blob or continuation moves because the surface
that produced it is gone. ADR 0060 already separated the machine from the
language: the VM is a heap substrate and a dialect is a lowering into it. What
this ADR retires is one lowering's front end, not the machine.

## Decision

**TypeScript is the only RLM authoring language. "Lashlang" names the
dialect-neutral IR and the VM that executes it, and nothing else.**

### What stays

The `lashlang` crate keeps everything below the surface: the AST, the linker,
the compiler, the bytecode format, the continuation format, the heap and value
model, the VM, the workflow graph, and the `lash-lashlang-runtime` engine
crate. These are the IR that TypeScript lowers into. They are not deprecated,
not on a clock, and not renamed — the name now means the IR, and renaming the
crate would be a cosmetic change that moves durable engine ids for nothing.

### What goes

The lashlang *surface*: the lexer, the parser, the canonical printer, `.lash`
files anywhere in the tree, and the lashlang prompt contract (its vocabulary,
cell tag, finish form and tool call path) in `lash-protocol-rlm`.

`RlmDialect::Lashlang` and `CompilationDialect::Lashlang` go, **and so do the
enums**. A two-variant enum reduced to one variant is not a simplification; it
is a compatibility reader wearing a type's clothes, and it keeps every match
arm, every wire field and every host selector alive to serve a choice that no
longer exists. One language id, `typescript`, spelled out, per ADR 0061's
naming rule, which survives its parent.

### No compatibility reader

A session record or bytecode blob that pinned the retired dialect is refused
with a typed incompatible-format error. Nothing decodes it, nothing coerces it,
and nothing falls back. This is ADR 0061's own cutover stance — no migration
decoders at any format boundary — applied to the ADR that stated it.
Deployments drain or recreate parked processes across the window, as they
already do when a durable format version moves.

### Reversal stance

A lashlang surface is not preserved against the possibility of wanting it back.
If one ever returns it is a **new dialect, authored against the IR of that
day**, with its own ADR, its own lowering and its own battery — not a restore
of deleted code against an IR that will have moved. That cost is understood and
accepted as the price of this ruling.

Ruled by Sam on 2026-09-13, following the processes-are-values design session
([ADR 0095](0095-processes-are-values-and-process-controls-are-tools.md)).

## Consequences

- **ADR 0061 is superseded in full.** There is no parity obligation, no
  doubled battery, no per-session dialect choice and no default to preserve.
  Its naming rule (`typescript`, spelled out, everywhere) and its cutover
  stance (no migration decoders) are carried forward by this ADR.
- **ADR 0063 is superseded in full.** One dialect cannot be prompted in the
  wrong one. The `DialectPromptVocabulary` indirection, the prompt walker and
  its carve-out register exist to police a boundary that is gone. Its one
  durable residue survives on its own merits and is restated below.
- **Substrate identifiers keep their spellings.** `lashlang_step`,
  `process:lashlang:v2:…` and `lashlang:effect:…` still name the engine, which
  is still the lashlang VM. They were carved out in ADR 0063 because renaming
  them moves durable identity; that reason is unchanged, and this ADR renames
  nothing. What changes is that they are no longer *foreign* words in a
  prompt — they are the substrate's name, under the only dialect there is.
- **The `__` namespace stays reserved.** `__typescript_runtime` and its
  siblings remain hidden from the model rather than renamed, for the same
  durability reason ADR 0063 gave.
- **Tool prose stays dialect-neutral by default, but the guard changes
  shape.** With one registered dialect, a registration check that refuses any
  registered dialect's identity would refuse the only correct spelling. Prose
  may name TypeScript; the token mechanism is retired with the second
  vocabulary. FIG-3021 owns the mechanics.
- **The workflow-graph lens loses its stated limit.** ADR 0037's lens laws
  demanded a canonical printer whose source → graph → source round trip is an
  exact textual fixpoint, and ADR 0061 scoped the lens to Lashlang because only
  Lashlang had that printer. With the surface retired the lens operates on the
  IR and the graph; a *TypeScript* canonical printer and its round-trip laws
  remain unbuilt and are separate future work with their own ADR. Host-facing
  features that reach through the lens are scoped accordingly.
- **"Lashlang" in prose means the IR and VM.** ADRs 0037, 0055, 0060, 0062 and
  0064 carry a dated amendment to that effect; the root `CONTEXT.md` glossary
  says it directly, and adds "TypeScript dialect" as the only RLM language.
  Where those ADRs describe Lashlang's *value semantics*, its isolation copies
  or its durable-boundary validator, they are describing the IR's semantics and
  are unchanged.
- **One breaking window, already scheduled.** The dialect tag leaves the
  session record, bytecode and wire in FIG-3019, which coordinates its format
  bump with FIG-2996 rather than spending a second window.

## Children

The arc is FIG-3015. This ADR is its first child and lands before any deletion.

- **FIG-3019** — `RlmDialect` and `CompilationDialect` lose the variant and the
  enums; no dialect field on sessions, bytecode or the wire.
- **FIG-3020** — lashlang crate: lexer, parser and canonical printer deleted,
  `.lash` gone, language tests re-authored in TypeScript.
- **FIG-3021** — `lash-protocol-rlm`: lashlang prompt contract and dialect
  registry deleted.
- **FIG-3022** — hosts and examples: every `RlmDialect::Lashlang` site and
  dialect selector removed.
- **FIG-3023** — runbooks: the lashlang-driven judged runbooks re-authored in
  TypeScript and re-judged.
- **FIG-3024** — docs and READMEs: lashlang means the IR; TypeScript is the
  authoring language everywhere.
