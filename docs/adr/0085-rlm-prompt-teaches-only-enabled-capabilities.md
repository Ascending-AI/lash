# ADR 0085: RLM prompts teach only enabled capabilities

Status: Accepted (2026-09-09)

## Decision

Render one tool catalogue per dialect. TypeScript uses typed declarations with
attached description, parameter and return notes; lashlang uses documented
awaited signatures. Preserve the host's descriptions, especially distinctions
between structured results and strings. Host-only operations and constructors
remain in the host surface; catalogue tools do not appear there again.

Prompt teaching follows the actual host abilities and prompt features. Process,
signal, sleep, trigger, image, label, type-literal and continuation teaching is
conditional on its capability. Aggregate await and ordinary context handling
remain available independently of decomposition. Empty sections are omitted.

TypeScript's standard library is named in one sentence. Exhaustive accepted
syntax, method inventories and rejection catalogues are removed: rejected
constructs carry actionable diagnostic hints. This supersedes tests requiring
all accepted TypeScript construct families to be named in every prompt; it does
not change ADR 0063's dialect vocabulary or durable-identity rules.

History's name, type and current count belong in the current-iteration tail.
Render its schema when steps or attachments make structured indexing useful.
Global-variable previews explain truncation only when this rendering actually
shortens a value; retained history outputs retain their local full-value paths.

The system introduction and Guidance use the same template for standard mode,
cell RLM and native-tool RLM. Transport differences remain in execution copy.

## Consequences

Small hosts pay only for applicable teaching. Prompt size and capability tests
pin this contract, alongside tool documentation, hint and transport snapshots.
This is a prompt rendering change only: execution, finish semantics, standard
mode tool JSON, persisted shapes and replay identities are unchanged.
