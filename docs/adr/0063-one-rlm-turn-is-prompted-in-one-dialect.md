# 0063: One RLM turn is prompted in its recorded dialect

## Status

Accepted.

## Context

Prompts must teach the source a session can execute. Source words and durable
machine identifiers have different owners, so choosing a dialect must not
remint execution identities.

## Decision

Every shared RLM prompt fragment uses the recorded `CellDialect` and its
`DialectPrompts`. `DialectPromptVocabulary` supplies the language, cell noun,
print and finish forms and notation. Tool paths and schema spellings
come from that same dialect. Bound-variable feedback, budget advice and
finalization describe its source. Programs travel through `execute_code`
with dialect-specific code-call instructions (ADR 0148).

TypeScript and Python are shipped front ends. The host chooses explicitly,
and the session pins its name at creation (ADRs 0138 and 0140). The protocol
parses no source; worker-installed kernel dialect packages lower it.

Clock, random and sleep are ordinary package operations supplied by the
dialect's service plugin. They have no privileged kernel form or host-read
kind. Their source spellings belong to the dialect; the model sees only the
tools explicitly put in its Tool Catalog. Internal `__` namespace filtering
is presentation, not authority or an automatic advertisement rule.

Tool descriptions and schema prose render verbatim. A host contributing
syntax-specific prose owns its consistency with the dialect it serves.
Machine engine names, effect identities and structural sites remain
source-neutral. Changing prompt vocabulary does not change their preimages.

## Consequences

A new dialect supplies prompt evidence along with its front end. Shared
prompts have no hard-coded source-language semantics. Hosts retain control of
their own prose, while traces keep the source label separate from admitted
machine and effect identities.

## Executable evidence

- [Dialect and prompt contracts](../../crates/lash-protocol-rlm/src/dialect.rs).
- [Prompt inventory](../../crates/lash-protocol-rlm/src/protocol/prompt.rs).
- [Dialect services](../../crates/lash-vm-runtime/src/dialect_services/mod.rs).
- [Process trace](../../crates/lash-vm-runtime/src/process/trace.rs) and
  [effect identity](../../crates/lash-vm-broker/src/identity.rs).
