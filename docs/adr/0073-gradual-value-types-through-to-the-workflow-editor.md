# 0073 — Gradual value types through to the workflow editor

- Status: Proposed
- Date: 2026-07-17
- Deciders: Samuel Galanakis
- Design map: Linear FIG-326 (wayfinder), decisions FIG-329…FIG-333, research FIG-327/FIG-328

## Context

The workflow-graph editor (`examples/workflow-graph-roundtrip`) plateaued because the
`code↔graph↔code` lens (ADR for the lens; `crates/lashlang/src/workflow_graph.rs`) carries **no
value-type information** to the editor. An adversarial design panel (independent, matching
file:line evidence) established the load-bearing correction:

**Lashlang the _language_ already has a value-type engine; the _lens/editor_ is what is typeless.**
- `TypeExpr` vocabulary — `crates/lashlang/src/ast.rs:574-599`.
- Forward inference with scopes `infer_expr_type` — `crates/lashlang/src/linker/pass_validation.rs:427-645`; propagation across `crates/lashlang/src/linker/lower_expr.rs`.
- Host op signatures already typed & checked — `ResourceOperationBinding{input_ty,output_ty}` (`linker/host.rs:161-165`); arg-check errors `IncompatibleOperationInput` (`lower_expr.rs:438-463`).

The real gaps are three unwired seams plus known inference holes, and one policy choice:
1. Tool schemas are **erased to `Any→Any`** at the runtime adapter (`crates/lash-lashlang-runtime/src/lib.rs:372-386`, `deferred.rs:111-125`) despite tool contracts carrying `input_schema`/`output_schema` (`crates/lash-sansio/src/tool_contract.rs:316-327`). No JSON-Schema→`TypeExpr` importer exists (only the reverse, `crates/lashlang/src/runtime/compiler/helpers.rs:300` / `process.rs`).
2. The lens **never links against a host environment** (`workflow_graph.rs:323-329`), so inferred types are discarded; `availableVars` rides on SSA `VersionState` (names only, `workflow_graph.rs:703-766`).
3. **Inference holes:** branch-join first-wins not union (`linker/type_helpers.rs:42-46`); `for`-binding `Any` over `list<T>` (`lower_expr.rs:210-218`); `state.x = …` doesn't update the field type; missing field → `Any`; `start`/`wait_signal` drop known types.
4. Today `Any` is **strict TS-`unknown`** (`crates/lashlang/src/trigger.rs:1022-1037`: assignable _to_ `Any`, not _from_), and there are no narrowing constructs, so turning on real signatures would make every unknown a hard error. Research FIG-327 measured this: ~4–6 of ~14 real programs would break, **0 genuine bugs — ~100% false positives**.

## Decision

Add value types **as a derived, read-only projection facet** the editor consumes, keeping the lens
laws intact and the language honestly gradual. Five ratified decisions:

### D1 — Guarantee: save-time rejection on *definite* errors (not staged)
The editor's end-state guarantee is **save-time rejection of workflows containing a _definite_ type
error**, with typed pickers/completion and advisory (non-blocking) diagnostics always beneath it.
"Definite-only" is what keeps rejection from false-positiving on unknowns. Typed LLM/subagent
outputs are in scope (see D3). Runtime schema validation (`schema_validation.rs:39-40`) remains the
exact safety net; a definite static mismatch blocks save, everything unknown is allowed.

### D2 — `Any` is consistent (gradual), not strict-`unknown`
Flip `is_resolved_type_assignable` (`trigger.rs:1022-1037`) so `Any` is **consistent with any
type** (Siek–Taha gradual `any`): any source is consistent with `Any`, and `Any` is consistent with
any target. Rejection fires only on a mismatch between two _known_ types. This makes the migration
**non-breaking** (the ~100% false positives never fire because unknowns pass) and the static layer
intentionally unsound _at `Any`_, with runtime validation as enforcement. **No narrowing/cast
constructs** are added.

### D3 — Tool-schema import, trusted signatures, bounded typed-outputs
1. **Adopt a JSON-Schema→`TypeExpr` importer** at the `lash-lashlang-runtime` bridge, replacing the
   `Any→Any` erasure. Mirror the existing reverse converter. Unrepresentable constructs degrade to
   `Dict`/`Any` — **never a hard error**. Reuse the remote-protocol `FromInputSchema` mirror
   (`crates/lash-remote-protocol/src/tools.rs:208`, `core_conversions/tools.rs:89`); align with
   Figments `output_contract` (capability-as-data, ADR 0026).
2. **Signatures are trusted axioms.** The type system believes the host-declared signature; existing
   runtime validation is the enforcement/blame at the boundary. No separate static conformance
   system.
3. **Bounded typed-outputs.** Type first-class `Type{...}` descriptors + `FromInputSchema` outputs
   via a **non-denotable, linker-only witness in `Binding`** (not a surface `TypeExpr` variant):
   - Eligibility = a **closed compile-time schema witness** (recursively foldable; refs resolve to
     declared aliases/constants). A `Type{ nested: Inner }` whose `Inner` is a runtime value
     (`WrapTypeLiteral`, `runtime/compiler/effects.rs:540,562`) is **not** closed → `Any`.
   - Cover **both direct syntaxes**: `Type{...}` literals and the record-shorthand decoded by the
     existing `parse_output_schema` / `$lash_type` vocabulary (`crates/lash-lashlang-runtime/src/typed_output.rs`).
   - The one contract-keyed rule: for a call whose contract is
     `ToolOutputContract::FromInputSchema{input_field, default_schema}`, output type = the shape of
     the closed witness in `input_field`; else `default_schema`; else `Any`. Key on the **contract**,
     never on tool names.
   - **Defer** stored-`Shape` variable propagation (graduates cheaply once D4's branch-join lands).
   - Prior art: Zod `z.infer`/TS `typeof` (staged boundary) + F# **erased** Type Providers (the bound).

### D4 — Bidirectional inference, close every hole, bounded loops
- **Bidirectional local inference:** variable types synthesize bottom-up (exists); **expected types
  flow top-down** from operation/slot signatures (Hazel / Pierce–Turner). Required for the editor's
  "what variable can fill this slot?" question.
- **Close every hole:** branch-join → proper `Union` (`type_helpers.rs:42-46`); `for`-element from
  `list<T>` (`lower_expr.rs:210-218`); `start`/`await` output type attached (`lower_expr.rs:287`,
  `pass_validation.rs:557`); `state.x = …` updates the field type; **missing field on a _known_
  `Object` → definite error** (not `Any`); binary-operator operand checking.
- **Bounded loop precision:** one forward pass + widen (`Union`/`Any`), **no fixpoint iteration**.

### D5 — Type facets in the graph contract
The lens projection gains a **full facet set, inline per-node, derived-read-only, schema-versioned**:
- typed `availableVars` → `[{name, type}]`;
- **expected type per argument slot** (from the bidirectional pass);
- **per-node diagnostics** (the definite-error list driving save-time rejection + inline underlines).
Facets are recomputed on every GET/reproject, **never PUT back**, and **excluded from
canonicalization/diffing**, so GetPut/PutGet and the canonical-source goldens are unaffected. Bump a
facet schema version independently of the graph schema version (`workflow_graph.rs:34-35`).

## Consequences

- Migration is non-breaking (D2); real signatures can be turned on without touching existing
  workflows.
- Typed LLM/subagent outputs — the highest-value slot — become checkable downstream (~94% of real
  corpus call sites; the rest degrade to `Any`).
- The lens laws are preserved because facets are derived and never authoritative.
- The static layer is deliberately unsound at `Any`; runtime validation stays the real guarantee.

## Out of scope

- Global / Hindley-Milner inference (Lamdu cautionary tale).
- General/unbounded dependent typing (process-parameter schema generics, conditional-descriptor
  correlation, schema combinators, type-level computation) — only the bounded rule above is in.
- Strict-`Any` + narrowing/cast constructs (rejected in D2).
- A runtime contract/blame soundness system (signatures are trusted; runtime validation enforces).
- "Narrow-A" enum/domain types on tool params as a standalone win (subsumed).

## Implementation phases

1. **Consistent-`Any` (D2)** — `trigger.rs` assignability + tests.
2. **Tool-schema importer (D3.1)** — JSON-Schema→`TypeExpr`, wire at the bridge, drop the `Any→Any`
   erasure.
3. **Inference hardening (D4)** — bidirectional + holes + bounded loop-widening.
4. **Bounded typed-outputs (D3.3)** — closed-witness metatype in `Binding` + contract-keyed rule.
5. **Facet projection (D4-expose + D5)** — run the checker with host context in the lens; emit the
   derived facet set; schema-version; exclude from canonicalization.
6. **Editor consumes facets** — type-filtered pickers, expected-slot hints, diagnostics/underlines,
   save-time rejection.

## References

- Linear FIG-326 (map) and FIG-327…FIG-333 (research + decisions, with full resolution rationale).
- Panel evidence: `scratchpad/valuetype-assertions.md`, `scratchpad/bounded-vs-expansion.md`.
- ADR 0026 (capability is host-supplied data); the workflow-graph lens ADR.
