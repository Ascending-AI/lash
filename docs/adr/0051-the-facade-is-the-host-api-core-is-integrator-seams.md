# 0051. The facade is the host API; lash-core's public surface is its integrator seams

Date: 2026-08-03
Status: accepted

## Decision

The `lash` crate is the supported host API, in full. `lash-core` keeps a public
item exactly when a named integrator class needs it, and for no other reason.
Everything else in `lash-core` is crate-private or reachable only through
internal seams the facade consumes. Public surface is a promise we test and
keep; anything we are not prepared to promise is not public.

The named integrator classes, each seeded by the interfaces an implementor must
write against, are:

1. **Store and durable-substrate implementors** — the store traits
   (`RuntimePersistence`, `SessionStoreFactory`, `ProcessRegistry`,
   `TriggerStore`, `AttachmentStore`, `LiveReplayStore`,
   `ProcessExecutionEnvStore`, `ProcessContinuationStore`, and their sibling
   store contracts) and every type their signatures reach.
2. **Effect-host implementors** — `EffectHost`, `RuntimeEffectController`,
   `AwaitEventResolver`, and their signature closure.
3. **Protocol and process-engine implementors** — `ProtocolSessionPlugin`,
   `ProtocolDriverPlugin`, `CodeExecutorPlugin`, `ProcessEngine`, and the other
   engine extension points, with their closure.
4. **Conformance-suite embedders** — everything
   `lash::testing::conformance` exposes, closed over its signatures, so an
   integrator can hold a custom backend to the same executable contract the
   built-in backends answer to.

Membership is decided by **transitive signature closure**, not by direct-use
scanning. A type that appears only in a public trait method's parameters or
return type is integrator surface, whether or not any repository code names it
today: `ExecRequest` exists because `CodeExecutorPlugin::execute_code` exists.
The narrowing measurement found 683 items that a direct-use scan classified as
removable but that the closure proves are load-bearing for implementors; the
closure rule is therefore normative, and any future tooling that classifies
surface must apply it.

The closure decides **members**, not only types. A type enters the closure in a
direction: an implementor either *produces* it (it appears in return position,
so the implementor must construct one) or *consumes* it (it appears in argument
position, so the implementor must read one), and a type reached through a field
inherits its owner's direction. A member of a closure type is integrator
surface exactly when the direction it serves makes it load-bearing:

- **Produce-side** — the constructors and builders an implementor needs to
  return a value of the type, including on types with public fields, because a
  struct literal is not a promise we can extend. Direction is per class, not per
  type: `RuntimeCommit` is consume-side for a store, which is handed one by
  `SessionCommitStore::commit_runtime_state`, and produce-side for a conformance
  embedder, which assembles one to hold that store to the contract — so its
  builders are integrator surface.
- **Consume-side** — the accessors an implementor needs to read the value it
  was handed. On an opaque type they are the *only* interface, so every one of
  them is load-bearing: `ProcessEngineRunContext` is what
  `ProcessEngine::run` receives, so its accessors are the engine contract.
- **Neither** — a member that only projects a value the implementor produces,
  or only mutates state the runtime owns, serves no direction. It is not
  integrator surface even when it sits on a closure type.

Direct-use scanning is as non-normative here as it is for types, and more
dangerous: a member can have no caller in this repository and still be the one
thing an implementor must call. `TurnContext`'s prompt mutators remain on the
facade turn builder; `TurnContextTransform::transform` receives and returns a
`PreparedContext` and never sees `&mut TurnContext`. `set_prompt_layer` remains
inherent for `lash-remote-protocol`, while the other prompt mutators belong to
the facade seam.

Members that are not integrator surface get one of two homes, chosen by who
holds the receiver:

- `pub(crate)`, or a seam trait re-exported through `lash_core::facade_support`
  when the facade needs it across the crate boundary, for receivers only the
  runtime and the facade ever hold.
- A public `lash::<domain>::<Type>Ext` extension trait, example-covered from
  birth, for receivers a *host* holds where the behavior is host convenience
  the core contract does not need. Same-named trait methods keep existing call
  sites compiling, so the migration is an import, not a rewrite.

The package-version constants (`lash_core::VERSION`, `SANSIO_VERSION`) are not
integrator surface. Compatibility between an integrator and the runtime is
expressed through trait contracts, data shapes, and the schema-version
machinery — never by gating on a package version.

## Why

Three defects in one week were unused-public-surface defects: a drain API with
zero callers concealed a wake-mark over-claim; an append precondition with one
since-deleted caller was read in opposite ways by two capable reviewers; an
orphaned retention lever meant deleting an unsafe deletion left a table with no
reclamation path. Surface nobody consumes drifts until its first consumer
discovers what it actually does. The example-coverage rule (the inventory and
its CI contract) makes such drift visible, but visibility alone would have us
writing examples against 5,424 public core items — most of which exist only
because the facade's implementation happens to live in another crate. Narrowing
visibility deletes no functionality: hosts keep everything through the facade.
It deletes *promises we never meant to make*.

The measurement behind this decision classified every public `lash-core` item:
2,848 are reachable by no integrator class (942 of them by nothing at all,
including the facade); 2,574 are integrator surface under the closure; 2 were
contested and are resolved above. The facade's own 6,821 items produced zero
trim candidates — the facade is already the deliberate API this ADR makes
authoritative.

## Consequences

- Anything consuming `lash-core` directly that is not one of the four classes
  is unsupported. The measured exceptions — 79 items held only by direct
  example or downstream-host use — are treated as **facade gaps**: each gets a
  facade home (or the consumer moves to an existing one) rather than a
  perpetual carve-out.
- The example-coverage inventory shrinks with the surface, and the coverage
  end-state ("every public API used and tested through our examples") is now a
  statement about surface we deliberately promise. Conformance suites remain
  the integrator classes' executable contract, and integrator-facing surface
  is exercised by integrator-class examples rather than shoehorned into host
  examples.
- Internalization proceeds in waves: first the zero-consumer items, then the
  facade-gap moves, then the remaining facade-only items. Each wave is
  breaking for direct `lash-core` consumers and carries `Breaking:` release
  notes naming the facade or seam replacement.
- New public items in `lash-core` must name their integrator class in rustdoc.
  An item that cannot name one belongs behind the facade.
- FIG-863's measured facade-seam floor is 3,193 `lash-core` rows, replacing
  wave D's 3,050–3,150 forecast; the count is an outcome of applying the rule,
  not a goal. Measured against the member-level closure above,
  455 of the 561 inherent members on retained `lash-core` root exports have a
  proven caller outside `lash-core`, and most of the remainder are load-bearing
  in a direction no in-repo crate exercises yet. A wave that moved them to hit
  a number would delete integrator ergonomics, not unkept promises. The core
  row count is an outcome of applying the rule, never the input.
- `lash-remote-protocol` converts wire DTOs to and from core types and cannot
  depend on the facade, because the facade depends on it. Its `core-conversions`
  feature is therefore a fifth seeding point for the closure alongside the four
  classes. It reaches 70 inherent members on retained root exports, 52 of them
  as the sole caller outside `lash-core`; all are retained core surface.
- The example-coverage inventory keys one row per **API item**, not per path
  (FIG-955), so "a `lash-core` row" now means an item with no facade projection.
  At that keying the surface is 7,516 items, of which **826** are reachable only
  through `lash_core`. The 3,193-row facade-seam floor above counted paths, and
  that path count has since grown to 4,233 (FIG-863 measured 3,193; #258 added
  949 reachability-closure rows and #244 one more). Of those paths, 3,407 name
  items the facade already re-exports, and each was carrying a second — often
  contradictory — disposition for the same contract. The rule is unchanged and
  the count remains an outcome rather than a goal, but a path count and an item
  count measure different things and must not be compared.
- **Per-path existence stays enforced.** Retiring a `lash_core::` re-export is
  breaking per the internalization bullet above, and item keying would have made
  it invisible (the retired path is an alias, not a row). So each row records its
  item's remaining public paths in `aliases`, derived from rustdoc the same way
  `availability` and `kind` are, and any path appearing or disappearing fails
  `scripts/check_api_example_coverage.py`. Only the *disposition* is centralized
  on the item; the path set is not, and a wave that internalizes core paths must
  still edit the inventory row by row.
- No justification may park a `lash-core` (or other facade-dependency) consumer
  behind a pending migration to the facade. The cycle this ADR names for
  `lash-remote-protocol` holds for every crate the facade is built on. The
  checker derives that crate set from the resolved dependency graph
  (`cargo metadata`) and reads the claim per sentence, so stating that a caller
  *cannot* migrate — the honest description of the cycle — is not flagged.
