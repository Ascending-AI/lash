# 0063: One RLM turn is prompted in its dialect

## Status

Partially superseded by [ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md).
The paired-language evidence and pinning obligations are superseded. The
multi-dialect prompt architecture remains accepted, clarified by Sam's
FIG-4276 ruling on 2026-09-30.

## Retained prompt contract

Every shared RLM prompt fragment uses the selected dialect's vocabulary.
`DialectPromptVocabulary` supplies the language name, cell tags, cell noun,
print call, finish forms and continue-as examples. Bound variables, read-only
variables, retries, budget advice and finalization must describe the source
that the model can actually write. Cell-delimiter advice is specific to the
selected dialect and exists only on the cell channel.

TypeScript is the only shipped dialect today. The retained prompt walker
checks its assembled fragments, including host surfaces and tool signatures,
against the retired authored-language markers. It keeps explicit carve-outs
for IR and VM identifiers. A new dialect supplies its own vocabulary,
signatures, schema spelling and prompt evidence; it need not match another
dialect's behavior. ADR 0096 records the extension contract and the current
TypeScript-specific prompt adapters that still need generalization.

The `__` namespace is reserved for internal runtime modules. They are hidden
from model-visible host-surface documentation. The journaled clock and random
module is `__lashlang_runtime`, with resource type `lashlang.Runtime` and host
operation `lashlang.runtime` (FIG-4020), regardless of source dialect.

Tool descriptions and schema prose render verbatim (FIG-4093). A host that
writes syntax-specific prose owns its consistency with the served dialect.
There is no cross-language prose ban or prose-token substitution mechanism.

## Durable identifiers and traces

`lashlang_step`, process identity families and `lashlang:effect:...` name the
IR and VM. They retain their spellings across source dialects. IR module refs
and source identity use the dialect-neutral atom `lashlang-ir`.

Cell execution traces name the source dialect in `language`; a compiled
process body executes IR and keeps the engine's language label. Event names,
JSONL filenames and graph APIs that name Lashlang continue to name the shared
machine. Hosts that assemble prompts own the same dialect consistency rule
for their examples and tutorials.

## Superseded history

The original decision coupled two installed languages, registered-but-inactive
cell recognition, session pinning and paired prompt evidence. Those obligations
were retired with the authored Lashlang language. The `{{...}}` tool-prose token
mechanism and its registration guard were removed by FIG-4093. The original
`__typescript_runtime` spelling was replaced by FIG-4020. None is a requirement
for future dialects.

The FIG-2505 process-environment v4-to-v5 window is historical. Current format
changes follow the pre-1.0 freeze and ADR 0115's 1.0 cut.

## Executable evidence

`no_assembled_prompt_fragment_carries_the_retired_surfaces_words` in
[the walker tests](../../crates/lash-protocol-rlm/src/dialect/prompt_walker_tests.rs)
checks TypeScript prompt fragments with the explicit IR/VM carve-outs.
The extension-session tests in `dialect.rs` exercise vocabulary and delimiter
selection without changing the IR, VM or execution request.
