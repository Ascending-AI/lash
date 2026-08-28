# FIG-2090: compiler-derived facade example coverage

## Verdict evidence

The prototype is feasible as an **advisory direct-call index**. It mechanically
proved 226 default-feature `lash::` facade function/method identities in three
real, compiled example packages. The result includes 124 re-exported items and
does not contain an item-to-example mapping.

It is not feasible as a replacement for FIG-861's complete `exercised-by`
inventory. Stock rustdoc cannot derive field access, general variant use,
struct literals, type/constant use, concrete trait-implementation selection, or
doctest calls. Those categories must be reported as `not derivable`, not
`uncovered`. The orchestrator can therefore choose between retaining this
narrow advisory signal or rejecting the mechanism if every facade item kind is
a hard requirement.

Here, `covered` means **a typechecked direct call exists in a compiled source
body**. It does not mean that the example executed the call. Dead branches can
count; runtime reflection cannot. Calls introduced by macro expansion are
deliberately excluded by rustdoc.

## Implemented prototype

[`scripts/api_evidence.py`](../../scripts/api_evidence.py) performs one
compiler-coordinated scrape and one mechanical join:

1. It imports the canonical identity, re-export, and primary-path logic from
   `scripts/api_surface.py`. The prototype uses the default-feature facade
   surface, currently 7,392 identities.
2. It reads Cargo metadata and selects three product example packages:
   `agent-service`, `agent-workbench`, and `slack-clone`. This fixed
   representative package set is not an item-to-example map.
3. Cargo requires source targets to opt in with `doc-scrape-examples = true`.
   The script creates a temporary `git archive` snapshot and adds that target
   setting mechanically to each selected package. It does not change a tracked
   manifest or example source.
4. It invokes Cargo once for `lash-runtime` and all selected examples with
   `-Zrustdoc-scrape-examples`. Cargo/rustdoc coordinate the private compiler
   identities; the script does not decode rustc's opaque `.examples` files.
5. It reads the scraped-example sections rustdoc attached to rendered `lash`
   function and method pages, then joins those page paths through the existing
   canonical alias sets. Calls through facade re-exports collapse onto their
   underlying canonical identity.
6. It fails if fewer than three packages are selected or any selected package
   contributes zero matched facade calls. Non-call inventory kinds are printed
   separately as `not derivable`.

The advisory workflow is `.github/workflows/api-evidence.yml`. It runs on
facade/example-affecting pull requests and `main`, publishes the report in the
job summary, and deliberately uses `continue-on-error`: unstable rustdoc drift
or an evidence gap must not become a release gate.

No mapping file or hand-maintained disposition entry was added or changed.

## Measured Lash result

Environment:

```text
rustc 1.97.0 (2d8144b78 2026-07-07)
cargo 1.97.0 (c980f4866 2026-06-30)
rustc/rustdoc 1.100.0-nightly (fb6531d55 2026-08-23)
cargo 1.100.0-nightly (e8cb624d5 2026-08-22)
```

The native scrape command, after mechanically adding the three target opt-ins
in a disposable worktree, was:

```console
$ cargo +nightly doc --no-deps \
    -p lash-runtime -p agent-service -p agent-workbench -p slack-clone \
    -Z unstable-options -Z rustdoc-scrape-examples
Scraping slack-clone ...
Scraping agent-workbench ...
Scraping agent-service ...
Documenting lash-runtime ...
Finished `dev` profile ... in 3m 42s
```

Joining the resulting HTML against the canonical default-feature surface gave:

| Result | Count |
|---|---:|
| Canonical facade identities inspected | 7,392 |
| Direct-call candidates (functions and methods) | 2,117 |
| Covered direct-call identities | 226 |
| Uncovered direct-call candidates | 1,891 |
| Covered identities defined outside the facade crate (re-exports) | 124 |
| Distinct contributing source files | 23 |

The 226 covered identities split into 4 plain functions, 24 named constructors,
195 methods rendered on concrete type pages, and 3 trait methods. Every selected
example contributed:

| Example package | Matched facade identities |
|---|---:|
| `agent-service` | 80 |
| `agent-workbench` | 146 |
| `slack-clone` | 72 |

Counts overlap across packages, so the contributor rows do not sum to 226.
Representative mechanically joined results include:

```text
lash::message_text
  identity lash::turn::message_text
  <- examples/slack-clone/src/bot/channel.rs

lash::persistence::reclaim_unreferenced_attachments
  identity lash_core::attachments::reclaim_unreferenced_attachments
  <- examples/agent-service/src/retention.rs
  <- examples/agent-workbench/src/main_sections/admin.rs

lash::TurnAddress::new
  identity lash_core::runtime::turn_control::TurnAddress::new
  <- examples/agent-service/src/routes.rs
  <- examples/agent-workbench/src/main_sections/state.rs

lash::persistence::SessionStoreFactory::open_existing_store
  identity lash_core::runtime::SessionStoreFactory::open_existing_store
  <- examples/agent-service/src/retention.rs
  <- examples/agent-workbench/src/main_sections/admin.rs
```

This confirms the important facade join: a call to an item defined in
`lash_core`, `lash_sansio`, `lash_tool_support`, or another dependency can be
rendered on its inline `lash` re-export page and then collapsed to the same
canonical identity used by the existing inventory.

## Per-item-kind result

| Facade inventory kind | Prototype result | Exact qualification |
|---|---|---|
| Plain function | **Derivable** for direct calls | HIR `Call` with a concrete `FnDef`; merely naming/coercing the function is not enough. |
| Associated function / named constructor | **Derivable** for direct calls | `Type::new(...)` is an ordinary associated-function call. Constructor is a presentation category in the prototype, not a separate rustdoc identity kind. |
| Inherent method | **Derivable** for direct calls | HIR `MethodCall` resolves to a type-dependent `DefId`. |
| Trait method declaration | **Derivable** at the selected associated-item identity | Three real facade trait-method identities were recovered. |
| Specific trait impl or overridden impl method | **Not derivable** | The scrape record carries no receiver type, selected impl, or concrete impl-method identity. A trait call is evidence for the associated item only. |
| Re-exported function/method | **Derivable at canonical underlying identity** | 124 covered results had an identity outside `lash::`; alias spelling is not preserved and aliases intentionally collapse. |
| Public field | **Not derivable** | Field reads/writes and struct-literal fields are HIR field/struct expressions, not `Call`/`MethodCall`. The current default surface has 3,047 field identities. |
| Unit/struct enum variant or pattern | **Not derivable** | Paths, struct-like construction, and patterns are not calls. The current default surface has 1,290 variant identities. |
| Tuple struct/variant constructor | **Partially collected, not joinable by this adapter** | Tuple-like constructors can have `FnDef` types, but rustdoc renders scrape evidence only on function/method documentation items. The current inventory identifies the struct/variant, not a separately rendered tuple-constructor function. |
| Struct, enum, or trait type | **Not derivable** | Type use is not a direct call. Named methods remain independently derivable. |
| Type alias, constant, associated constant/type | **Not derivable** | These are path/type uses rather than function calls. |
| Doctest call | **Not provided by Cargo's scrape workflow** | Cargo scrapes opted-in targets. It does not feed rustdoc's extracted doctest crates into this call map. |

These walls come directly from rustdoc's compiler visitor: it handles only HIR
`Call` and `MethodCall`, rejects macro-expanded expressions, requires a concrete
`FnDef`, and writes `DefPathHash` plus source spans.
([pinned scraper source](https://github.com/rust-lang/rust/blob/e7769602aca3770e8d8ea55716becb22e839a579/src/librustdoc/scrape_examples.rs#L130-L269))
The payload contains no receiver/impl metadata.
([payload types](https://github.com/rust-lang/rust/blob/e7769602aca3770e8d8ea55716becb22e839a579/src/librustdoc/scrape_examples.rs#L59-L109))
Rustdoc gates rendered scraped examples to function and method items.
([render lookup](https://github.com/rust-lang/rust/blob/e7769602aca3770e8d8ea55716becb22e839a579/src/librustdoc/html/render/mod.rs#L797-L829))

## Output-route investigation

### Native scrape artifact and rendered HTML

The private map has this shape:

```text
DefPathHash
  -> canonical absolute example source path
    -> display path, URL, edition, binary flag
       + call span
       + callee span
       + enclosing-item span
```

Rustdoc serializes it with rustc's `FileEncoder` and reads it with
`MemDecoder`; it is compiler-private, version-coupled binary data, not a stable
exchange format.
([encoder/decoder](https://github.com/rust-lang/rust/blob/e7769602aca3770e8d8ea55716becb22e839a579/src/librustdoc/scrape_examples.rs#L323-L371))
Parsing the rendered HTML is still unstable and lossy, but lets rustdoc perform
the `DefPathHash` join itself. It is the smallest prototype that avoids copying
rustc serialization or introducing a mapping file.

Cargo's native route requires each source target to set
`doc-scrape-examples = true`; Cargo's own regression covers re-exported methods
and the coordinated target build.
([Cargo documentation](https://doc.rust-lang.org/nightly/cargo/reference/unstable.html#scrape-examples),
[Cargo regression](https://github.com/rust-lang/cargo/blob/e8cb624d5701824f46a2ec5873cfd59ee3d2f66c/tests/testsuite/docscrape.rs#L53-L134))
Because Lash examples are separate workspace packages rather than Cargo
`[[example]]` targets of `lash-runtime`, the temporary manifest opt-in is
necessary. The prototype derives it from Cargo target metadata.

### Rustdoc JSON

Rustdoc JSON describes declarations, not resolved body uses. Its root exposes
an item index, path summaries, and external crates. A function exposes its
signature and `has_body`, not HIR or a call graph.
([JSON root](https://github.com/rust-lang/rust/blob/e7769602aca3770e8d8ea55716becb22e839a579/src/rustdoc-json-types/lib.rs#L120-L146),
[function schema](https://github.com/rust-lang/rust/blob/e7769602aca3770e8d8ea55716becb22e839a579/src/rustdoc-json-types/lib.rs#L1206-L1225))

Local confirmation used:

```console
$ cargo +nightly rustdoc -p docs-snippets --lib \
    -Z unstable-options --output-format json -- \
    --document-private-items
```

The 1,934,399-byte result had format version 61, 1,911 indexed declarations,
and 4,349 path summaries. Searching its schema/data found no body, HIR, call,
or use graph. `--document-private-items` expands declaration visibility but
does not expose calls inside those declarations.

Raw HIR therefore does not offer a separate stable route: the evaluated
scrape-examples pass is the rustc-internal HIR visitor that already implements
the narrow call collection. Per the ticket scope, removed save-analysis output
was not pursued.

### Doctests

`cargo doc -Zrustdoc-scrape-examples` compiles opted-in Cargo targets, not
rustdoc's extracted doctest crates. Rustdoc's lower-level `--scrape-tests`
workflow refers to compiled test source and does not make Cargo persist and
re-feed extracted documentation blocks.
([rustdoc workflow](https://doc.rust-lang.org/nightly/rustdoc/unstable-features.html#--with-examples-include-examples-of-uses-of-items-as-documentation))
A custom doctest persistence/recompile pipeline would be a new compiler
integration, not evidence available from this prototype.

## Accuracy and operational limits

- False positives relative to runtime execution are expected: a compiled call
  in dead or unexercised code still counts.
- False negatives relative to semantic API use are expected for all non-call
  uses, function-pointer coercions, macro-expanded calls, dynamic dispatch, and
  the item kinds marked not derivable above.
- The HTML adapter depends on nightly rustdoc markup. Failure is explicit and
  advisory; a zero-contribution package makes the command nonzero.
- Results are per selected feature configuration. This prototype intentionally
  uses the default-feature canonical surface and default example builds; it
  does not claim all-feature coverage.
- The temporary archive uses committed `HEAD`, so local uncommitted example or
  manifest edits are deliberately excluded from evidence.
- Source paths are compiler-derived from rustdoc and normalized back to
  repository-relative paths before reporting.

The representative native build finished in 3m42s on this machine. The exact
end-to-end prototype command took 300.99s and 1,191,840 KiB maximum RSS: its
compiler scrape was 275.47s and its cached canonical-inventory/adapter work made
up the remainder. Earlier source-warm measurements were 189.01s after
invalidation and 0.39s when fully fresh. This is reasonable for an advisory,
path-filtered 16-vCPU CI job with a 15-minute timeout, but not cheap enough to
place on a required critical path.

## Reproduction

From repository root:

```console
$ . ./env.sh
$ python3 scripts/api_evidence.py
```

The command prints compiler-derived covered identities and source files,
uncovered direct-call candidates, and every non-derivable inventory category.
The checked representative result must have nonzero contributor counts for all
three default example packages.

Supporting checks:

```console
$ python3 -m py_compile scripts/api_evidence.py
$ python3 scripts/lint_docs.py docs/agents/fig2090-findings.md
$ git diff --check
```

## Recommendation to the orchestrator

Retain the prototype only if a mechanically derived, advisory direct-call
index is independently useful. Do not use it to replace or weaken FIG-861's
complete coverage contract: doing so would silently redefine fields, variants,
trait impls, and doctests out of existence. If the acceptance criterion remains
one compiler-derived `exercised-by` answer for every facade item kind, record
the approach as infeasible with current stock Cargo/rustdoc output.
