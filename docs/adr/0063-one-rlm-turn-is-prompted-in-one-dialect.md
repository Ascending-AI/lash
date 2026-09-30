# 0063: One RLM turn is prompted in its dialect

## Status

Accepted.

## Context

An RLM prompt teaches the source the model can write. Shared IR and VM names
also appear in tool identities and traces, so authored syntax and machine
identifiers need distinct rules. TypeScript is the shipped source dialect;
[ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md) owns that selection.

## Retained prompt contract

Every shared RLM prompt fragment uses the selected dialect's vocabulary.
`DialectPromptVocabulary` supplies the language name, cell noun, print call,
finish forms and continue-as examples. The dialect also supplies cell tags,
tool signatures and schema spelling. Bound variables, read-only variables,
retry feedback, budget advice and finalization describe that source. Delimiter
advice appears only on the cell channel and uses that dialect's delimiter.

The `Dialect` and `DialectSession` contracts permit another front end to
supply its own vocabulary and lowering into the shared IR. Its semantics and
coverage do not need to match TypeScript's. The shipped TypeScript adapter
owns its syntax-specific prompt fragments.

The `__` namespace is reserved for internal runtime modules. Prompt inventories
hide modules whose first path segment starts with `__`. The clock and random
module is `__lashlang_runtime`, with resource type `lashlang.Runtime` and host
operation `lashlang.runtime`, independent of the source dialect.

Tool descriptions and schema prose render verbatim. A host contributing
syntax-specific prose owns its consistency with the dialect it serves.

## Durable identifiers and traces

`lashlang_step`, process identity families and `lashlang:effect:...` identify
IR and VM execution. IR module references and source identity use
`lashlang-ir`. Source vocabulary does not rename durable identity preimages.

Cell execution traces label the source dialect in `language`; compiled process
bodies execute IR and keep the engine's language label. Event names, JSONL
filenames and graph APIs that name Lashlang identify the shared machine.

## Alternatives considered

Hard-coding source syntax in shared prompts couples every prompt consumer to
one front end and lets examples disagree with the parser. Vocabulary and
signature rendering keep those choices with the front end.

Renaming durable machine identifiers for each source dialect changes replay
identity without changing execution. Machine names remain dialect-neutral.

Rewriting words in host prose risks changing descriptions and schema text.
Verbatim rendering leaves that prose with its author.

## Consequences

A new dialect supplies prompt evidence as well as lowering. The TypeScript
walker checks assembled fragments and explicitly allows machine identifiers.
Hosts assembling examples and tutorials apply the same dialect-consistency
rule to their own contributions.

## Executable evidence

- [Dialect and session contracts](../../crates/lash-protocol-rlm/src/dialect.rs#L33),
  [vocabulary](../../crates/lash-protocol-rlm/src/dialect.rs#L308) and the
  extension-session tests in that file cover vocabulary and delimiter selection.
- [Prompt inventory filtering](../../crates/lash-protocol-rlm/src/protocol/prompt.rs#L21)
  hides the internal namespace.
- [Prompt walker](../../crates/lash-protocol-rlm/src/dialect/prompt_walker_tests.rs#L65)
  checks source wording with explicit IR and VM exceptions.
- [Cell trace](../../crates/lash-protocol-rlm/src/protocol/driver.rs#L480),
  [process trace](../../crates/lash-lashlang-runtime/src/process/execution_trace.rs#L469)
  and [IR identity](../../crates/lashlang/src/artifact_identity.rs#L6)
  keep source labels separate from machine identities.
