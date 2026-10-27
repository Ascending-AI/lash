# 0149: Released helpers keep exact code and upgrade explicitly

Status: Accepted. Governs helper availability in ADR 0139's dialect and
parked-root paragraphs, and helper upgrade operations in ADR 0115 §8.
Release-check evidence: lash-kernel-releases/src/tests.rs (FIG-7504).

## Context

Saved Documents and callables name build helpers by exact FunctionId. Shipping
only current definitions deletes code those objects still need. A helper body
change can therefore orphan an unchanged publication. Stable symbolic runtime
references would also require new rules for anonymous closures, defining Links,
brands and crash recovery. The code identity already supplies the required pin.

## Decision

Keep exact FunctionIds in Documents, bodies, values and parked state. Retain
every released definition in an append-only, checked-in helper archive. Retain
native declarations, charge formulae, native versions, lifted anonymous bodies,
exact callee edges and release labels with the kernel bodies. Authenticate each
member's content identity before checking each library's closed member set and
HelperLibraryId. Ordinary builds read checked-in data and write a deduplicated
candidate only to OUT_DIR. The explicit release recorder commits new artifacts.
It never changes a historical release file. See the archive format reference in
`crates/lash-kernel-releases/archive/README.md`.

A permanent ledger pairs public names by package-qualified HelperId, such as
`typescript:ts.get_computed`. It records full contract revisions, every release's
implementing FunctionId and removals as tombstones. HelperIds are upgrade
metadata only. They never enter Documents or runtime values. Anonymous closures
have no HelperId. Changed contracts append revisions; tombstoned names cannot
return with a different contract. Contracts include signature, errors, guard
unit and full limit formula, static and callback effects, expression/action
call form, kernel version and branded representation guarantees.

Native implementations are identified by package, name and native_version.
Keep each Rust implementation while a supported archived declaration binds it.
A changed answer requires a new native version, even if a finite fixture corpus
misses the change. Charge changes remain exact definition changes and require
the existing charge evidence; contract equality does not prove equal behavior.

Publication revisions are immutable and pinned by their content. A helper
release does not rewrite them. An owner may explicitly upgrade a publication:
pair helpers through the ledger, require equal contracts, rewrite exact
references bottom-up and create a new publication revision with a visible diff.
Runs retain their original revision by default. Run upgrades additionally need
a safe point over every retained executable coordinate, including closures and
pending completions. Changed branded constructors need representation migration
or refusal. Recompilation from source remains an optional owner choice.

A checkpoint's HelperLibraryId is an availability fast path. When it differs,
check exact reached FunctionIds against current code plus the archive. Parent,
worker and broker must bind exact versions, preserve defining Links, and admit
only nodes that cover that reach. Interpreter retirement still follows ADR
0106/0115 migration and support policy. Retaining bytes does not retain an
unsupported interpreter or service contract. Retirement or security revocation
is explicit support policy with typed refusal, never silent substitution.

The archive starts with this change during pre-1.0 development. Reset it at the
1.0 cut with the stored formats. After 1.0, release history is append-only.

## Consequences

Phase 1 records the archive, ledger and release checks, and adds versioned
native construction. It changes no runtime archive lookup or upgrade behavior.
Until phase 2 installs historical definitions in parent, worker and broker,
the existing MissingHelper refusal remains. Phase 3 implements explicit
publication and run upgrades. Helper operation imports are a separate change.

Definitions deduplicate by exact FunctionId. A changed callee can change many
caller hashes, so release evidence measures added definitions and compressed
and uncompressed bytes. Archive assembly cost is reported separately from
runtime cold-load costs, which belong to phase 2. Historical native versions
also require version-specific fixtures. Finite charge and behavior corpora are
evidence for their cases, not a proof for all inputs.
