# One RLM turn is prompted in one dialect

## Status

Accepted.

## Context

The RLM protocol grew up with exactly one execution language, so its prompt was
written in that language's words everywhere: the cell tag, the print call, the
finish form, the noun for a unit of code, the call path under which a tool is
offered. Adding a second dialect made the execution section dialect-aware and
left every fragment assembled around it — bound variables, read-only variables,
tool documentation, budget escalation, the final-answer instruction, the
`continue_as` doc, the truncation notices, the retry copy — speaking the first
dialect. A TypeScript session was therefore told, in one prompt, to write
`<typescript>` cells and that its variables were "already bound in lashlang" and
should be read "in `<lashlang>` blocks".

That is not a cosmetic defect. A model cannot follow both instructions; the
judged battery caught one spending reasoning tokens reconciling the
contradiction, and the failure mode in production is worse than a visible error:
the model follows whichever half it believes, the turn succeeds, and the row's
evidence carries a label its content disagrees with.

The reverse direction exists too, and it is the one nobody looks for. The
TypeScript lowerer resolves `Date.now()` and `Math.random()` through a host
module named `__typescript_runtime`, bound by
`lashlang_host_environment_from_tool_catalog` — which builds the host
environment for *every* dialect, Lashlang included. The prompt's host-environment
section advertises every typed module operation it finds, so a Lashlang reader
was handed `await __typescript_runtime.now(any)? -> float`: an internal
identifier, named for the other dialect, in a section that exists to tell the
model what it may call.

## Decision

**Every fragment of an assembled RLM prompt is written in the session's own
dialect, in both directions, and one executable walker enforces it.**

1. The dialect trait owns the words. `DialectPromptVocabulary` carries the
   language name, cell tag, cell noun, print call and statement, finish form and
   continue-as forms; `tool_call_path` resolves each tool under the dialect's own
   binding. A shared fragment reads its words from the vocabulary rather than
   spelling one dialect's syntax inline.
2. The walker (`dialect::prompt_walker_tests`) renders every fragment the crate
   contributes, for both dialects, and fails on any word belonging to the other
   one. Bound variables are rendered through the dialect's *own session*, which
   is the path a served turn uses; rendering them through the renderer directly
   proved only that the plumbing compiles.
3. **Substrate identifiers are not model-visible.** An identifier the substrate
   needs for its own lowering or durability is hidden from the prompt rather
   than renamed, because renaming moves a durable identity. Modules under the
   reserved `__` namespace are never advertised by the host-environment section.
4. Where hiding is impossible because the model genuinely receives the
   identifier in its data, the spelling is **carved out** — listed explicitly,
   with its reason, in the walker's `SUBSTRATE_CARVE_OUTS` and here.
5. **A cell of a registered-but-inactive dialect is recognized, never read as
   prose.** Extraction knows every registered dialect's tags, executes only the
   active one's, and names the mismatch on the first iteration. A scanner that
   knows only the active tags turns a mis-dialected reply into an unbounded
   re-prompt: the model is asked to finish, answers with the cell it was told to
   write, and the execution fence never fires because extraction never yields a
   cell to fence.

### The carve-out list

| Identifier | Why it may cross dialects | Disposition |
| --- | --- | --- |
| `lashlang_step` | The model-visible `history` variable really does contain `kind: "lashlang_step"` in both dialects: `RlmHistoryItem` is one serialized type, and the session-graph event ids are `lashlang_step_<turn>_<iteration>` (`protocol/driver.rs`). A prompt that said `typescript_step` would disagree with the data the model receives — the exact defect class this ADR closes | Carved out. Renaming both sides is a durable payload change, tracked separately |
| `process:lashlang:sha256:…` | A durable process id, and part of journal identity. Every dialect's processes are compiled against the Lashlang VM substrate, so the substrate's name is in the id. A host can see it through its own work API | Carved out. The *label* half of the same question — what a rendered transcript calls the code — reads the session's recorded dialect instead |
| `__typescript_runtime` | The module path is embedded in every lowered TypeScript program, including the persisted bodies of durable processes that must still resolve when a worker wakes them after a restart. The host operation ids (`typescript.runtime.now`) reach the effect journal | Hidden, not carved out: nothing about it has to reach a model, so the prompt omits it and the durable identity does not move |

## Consequences

- A new prompt fragment must take a vocabulary, or the walker fails the first
  time the two dialects disagree about it. This is the intended cost.
- The `__` namespace is reserved for substrate bindings across the lowerer and
  the host catalog. A host module that a model is meant to call must not use it.
- A future dialect adds one vocabulary and one marker list; nothing else in the
  assembly changes.
- The carve-out list is a debt register, not a permission. Each row names what
  would have to change for the entry to disappear.
- Hosts assemble prompt copy of their own (the Agent Workbench's tutorials, for
  instance). This ADR binds the substrate; a host that injects code examples
  owns the same rule for its own copy, and the reference hosts carry the
  matching fixture.
