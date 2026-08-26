#!/usr/bin/env python3
r"""Enforce dispositions for Lash's host-facing public API surface.

The public surface comes from rustdoc JSON, not from the inventory being
checked.  This keeps the oracle independent: adding or removing an export
changes the compiler's view and requires an explicit inventory update.

`lash-runtime` is walked through its public module tree; every path a host can
write is a named export.  `lash-core` cannot be walked that way -- its nested
public modules are runtime internals that happen to be reachable -- so its
surface is the root exports plus their *reachability closure*: every
core-owned type a root export hands to a host through a public signature.
Those types are keyed by the compiler's canonical path, which is often rooted
in a `pub(crate)` module and therefore appears nowhere in a root walk, even
though every public method on the value is callable.  See
`core_reachable_types`.

The unit of *disposition* is the **API item**, not the path.  A single item is
usually visible under several paths -- `lash::Session` and `lash_core::Session`
are one struct, and a reachability-closure type is often also a facade export --
so every path is keyed by the compiler's canonical identity for what it names,
and paths that share an identity are one inventory row.  That row lives at the
item's *primary* path (the facade path a host would write, when one exists).
Two rows for one item are therefore a contract contradiction, not a duplicate:
see `api_items` and the contradiction error in `item_errors`.

The unit of *existence* is still the **path**.  An alias carries no verdict, but
it is a promise: ADR 0051 makes retiring a `lash_core::` re-export breaking for
direct core consumers, wave by wave.  So each row records its item's remaining
paths in `aliases`, exactly as `availability` and `kind` are recorded -- derived
from the compiler, stored so that a change has to be acknowledged rather than
noticed.  Adding, removing, or moving any path fails the gate; only the
*disposition* is centralized, never the path set.  `--dump-surface` prints the
same projection the check compares against.

Evidence prose carries nine lints, because the prose is the evidence:

* No machine-local paths.  `/workspace/...`, `~/...`, and `C:\...` are one
  developer's checkout, not a contract another reader can verify.  Evidence
  must be repository-relative; `./`-prefixed paths are fine.
* No impossible facade migrations.  A crate the `lash` facade is built on
  cannot depend on the facade, so a justification may not park a consumer in
  such a crate behind a pending migration to it.  ADR 0051 names this cycle for
  `lash-remote-protocol`; the rule holds for every crate the facade is built on,
  and the set comes from `cargo metadata`, not a hand-written list.  Detection
  is per sentence and negation-aware: stating that a caller *cannot* migrate is
  the honest description of the same fact, and the cited crate must be named in
  the sentence making the claim so the error blames the right path.
* No tautological assertion anchors.  `size_of::<T>() > 0` holds for every
  non-ZST, so an anchor on one proves the path resolves and nothing else.  An
  item whose only evidence is a tautology is undispositioned, not exercised.
* No uninformative assertion anchors.  An anchor is evidence only if the line it
  quotes says what was observed.  Two shapes say nothing: the opening line of a
  multi-line assertion, which is the macro call and no operands -- `assert_eq!(`,
  or the `assert!(matches!(` the FIG-955 round's tautology lint could not see --
  and an anchor quoting a prefix of its line, which can hide the whole assertion
  behind `assert!(`.  See `uninformative_assertion`.
* No syntax-only exercise claims. Imports, type construction/declarations, and
  variant patterns prove only that an item resolves. They do not exercise its
  contract; a `used-unasserted` disposition may retain that reachability. See
  `perfunctory_exercise`.
* No inherited callback assertions. A fluent setup call cannot borrow a closure
  operand or match guard as its assertion: those lines can observe an unrelated
  callback while making every call in the surrounding chain look asserted. See
  `unrelated_fluent_assertion`.
* No reason that describes a disposition the row does not hold.  "Add X to the
  example and assert its result" is the instruction an `unused-add` row carries;
  on a row that records real usage and a real assertion it is simply false, and
  it hid 904 rows' actual contract behind boilerplate.  The same goes for the
  `used-unasserted` wording on a row that names an assertion.  See
  `stale_disposition_reason`.
* No unconfirmable citations.  Prose cites a *symbol* --
  `crates/x/src/y.rs::Type::method`, plus a verbatim `#`snippet`` matched inside
  that symbol's span when the target is a statement rather than the symbol
  itself -- and the checker parses the cited file at gate time to resolve it.
  The citation has to be about the item: the item's own name, or the type that
  owns it, must appear in the symbol.  64 rows written earlier in this arc cited
  real lines that never mentioned the symbol, which reads as verification and is
  not.  The citation also lands on code, sits in the function the prose names,
  and -- where a row cites evidence to reject it, or admits the resolver cannot
  tie it -- answers the inverse claim instead.  Every citation in every reason is
  read: FIG-1223 scoped this to internal dispositions and rows naming the ticket,
  and FIG-1526 re-anchored the 837 rows that scope was hiding and removed it.
  Line pins are rejected outright: they broke on every rebase that touched an
  anchored file and rotted silently whenever the shift happened to land on code,
  so FIG-1550 converted all 2,831 of them and deleted the shape.
  `prose_citations_recorded` pins the citation population, because dropping a
  `::symbol` is the one edit that takes a row out of this check.  See
  `prose_citation_defect` and `symbol_spans`.
* No missing repository paths.  A `crates/...`, `examples/...`, `runbooks/...`,
  `scripts/...`, or `docs/...` file cited in reason prose must still exist in
  the repository; the `::symbol` a citation carries is metadata and does not
  change which file must exist.  See `missing_repository_path`.

Evidence is tiered, and the tier is the *path shape* of the anchor rather than a
word in the prose.  Four tiers exist, strongest first:

1. `example-host` -- an example's own host code.  It proves a host needs the API
   to do a real job.
2. `example-test` -- a test module inside an example, or a file under an
   example's `tests/`.  Callable and asserted, but it can be circular: the test
   exists because the API does.
3. `crate-src` -- another workspace crate's `src/`.  Load-bearing internally,
   which is the right bar for an internal seam and the wrong bar for host API.
4. `workspace-tests` -- a `tests/` directory in a crate.  It proves nothing
   about demand; a probe usage in a test is exactly what let a dead session
   picker look alive (FIG-1223).

Test code is found the way the compiler finds it, not by how the file reads: a
`#[cfg(test)]` block, a `tests`-shaped path, and a file some *parent* declares
for tests are all test code.  The declaration is the shape that hides -- the gate
is in the parent, the file itself looks like shipped source -- so all of its
spellings count: any predicate that gates on `test` (`all(test, ...)` included,
`not(test)` excluded), the gate and the `mod` on one line, and `#[path]` sending
the module somewhere its name does not predict.  93 files here are test code only
by declaration.

`DISPOSITION_TIERS` records which tiers each disposition may anchor in, so no
row can blend them, and `tier_breakdown` prints the standing distribution on
every run.  The `example-test` population is *ratcheted* by
`EXAMPLE_TEST_TIER_RATCHET` rather than migrated: each row there is a per-row
design question ("should the example's host code use this?"), so the number may
only fall, and it falls in a diff a reviewer can see.

Two dispositions describe internal seams, because `#[doc(hidden)]` support
modules are real internal API and pretending otherwise is what turned one of
them into an amnesty channel (FIG-1223):

* `internal-consumed` -- another crate's shipped code needs this item.  The
  justification is not prose: it is a `crate-src` anchor in a crate other than
  the one that defines the item, checked by `reference_exists` on every run.  A
  dead item cannot produce one, and a fabricated one has to name a file and line
  that do not exist.  The claim here is *dependency*, not *exercise*, but an
  import is *not* an anchor: ruled for FIG-1223, a `use` line resolves whether or
  not anything in the crate needs the item, and four rows anchored on `pub use`
  were citing the declaring crate's own re-export.  A trait-impl signature *is*
  one, ruled the same way: `impl EffectHost for X { async fn
  retire_effect_journal(` is a crate binding itself to the item's contract, which
  is the strongest form the dependency claim takes.  A *member*'s anchor has to
  tie its line to the type that owns it --
  qualified on the line, or reached through a receiver that *resolves* to that
  type -- because `reference_exists` is a substring match and a leaf name matches
  by coincidence.  The quoted source text, not its incidental line number, is
  the durable identity: documentation or formatting above an unchanged consumer
  relocates the anchor within the same file before these semantic checks run.
  Resolution is textual and hop by hop (`type_facts`): the
  `impl` block types `self`, struct fields and method return types carry the
  chain forward, `type` aliases are followed, `impl Trait for Type` ties a
  receiver to the trait that owns the member, and a variant's declared payload
  types the binding a `if let Enum::Variant(x)` introduces.  The expression is
  assembled across continuation lines, because a fluent chain is written down the
  page and its receiver is rarely on the anchor's own line.  A field written in a
  literal is judged by the literal it sits in, since adjacent literals write one
  field name for two types, and an anchor inside a `struct`/`enum`/`trait` body
  is rejected outright: a declaration of a crate's own same-named member is not a
  consumer of ours.  Naming no rival is no defence: prelude types, file-local
  types and path-qualified rivals cannot be named, so a receiver that resolves
  elsewhere -- or nowhere -- is a defect on its own.  Nor is a bare occurrence of
  the name: with nothing qualifying it, no receiver carrying it, no literal
  containing it and no implementation declaring it, the line is a coincidence --
  that branch was passing a field name inside a SQL string and a `let session_id`
  in an unrelated test.  Nor is a *declaration* of the item a consumer of it, at
  any level: a file that declares the type is where the type lives, whatever path
  the ledger keys it under, unless the line writes the path and so names another
  crate's type explicitly.  See `declaration_anchor_defect`.  See `member_anchor_defect`.  The same predicate decides
  the *search* for a consumer, not only the check on an anchor already written; a
  name-based search answers a different question and files live API as dead.  A
  consumer that cannot be tied that way is recorded in prose under an `unused-*`
  disposition, so the dispositions that claim checked evidence keep meaning it --
  and where *any* earlier round tied one -- by anchor or in prose -- the row keeps
  its candidate in prose instead of hardening into a removal verdict, because a
  resolver that fails closed must fail into a question, never into a deletion
  instruction.  Fallible chains are read through their unwraps (`?`, `.await?`),
  a qualification has to use the row's *own* container (`TurnFinish::FinalValue`
  is not `TurnEvent::FinalValue`), and the crate that declares an item is found
  from its source rather than from the path root the ledger keys it under.
* `internal-test-only` -- the only consumers are tests.  The anchor must sit in
  a test tier, and the reason says so; an item that is not already behind the
  `testing` feature -- `lash_core::testing` or `lash_core::test_support`, see
  `FEATURE_GATED_TEST_HOMES` -- carries a `Relocate:` note, because "a test
  imports it" is exactly the level of justification that let a dead session
  picker look alive.

An `unused-justify` reason may cite a *successor ticket* as the row's future
consumer, and that is the one shape of justification that ages badly: a ticket
number is a promise, and a promise nobody is holding is how a row stays
`unused-justify` forever.  Ruled 2026-08-19 (FIG-1537), the citation is admissible
only when **both** halves hold: a conformance suite already exercises the surface
*runnably* -- so the claim is about a missing in-repo caller, never about
untested code -- **and** the named successor ticket carries the example in its own
`done-when`, so a person is accountable for retiring the row.  A successor
citation missing either half is not a justification, it is a deferral, and the
honest disposition for it is `unused-add`.  The rule is narrow on purpose: it
buys one release's grace for a surface whose only in-repo caller has not landed
yet, and it buys nothing for a surface nobody has committed to calling.

`unused-remove` verdicts leave a tombstone.  `[[removal_verdict]]` records every
item that has ever carried one, and a verdict may only be discharged by actually
removing the item -- never by moving it.  Commit `678d567bf` moved 126 items
behind a doc-hidden module and deleted their 444 rows in the same diff, so the
written verdicts simply evaporated; `removal_verdict_errors` is the rule that
would have failed it.  Reappearing under another path requires an explicit
`superseded_by` disposition change in the same diff, and the tombstone count is
pinned by `removal_verdicts_recorded` so deleting history is a visible edit
rather than a silent one.

Two exclusions remain deliberate, and each is enforced structurally rather than
by a hand-maintained list:

1. Items in nested `lash-core` public modules that no publicly reachable
   signature mentions.  They are internals; if one becomes host-visible it
   enters through the reachability closure automatically.  The `#[doc(hidden)]`
   support modules at the crate root are the exception and are walked in full:
   they are the workspace's internal cross-crate API, `gated_core_modules`
   records them in the inventory, and a hidden root module missing from that
   list fails the gate rather than escaping it.
2. Items owned by another crate and not re-exported by `lash` or `lash-core`.
   Each crate answers for its own surface.

`#[doc(hidden)]` is no longer one of them.  Hiding an internal seam from
host-facing rustdoc is a legitimate *documentation* choice; whether the ledger
answers for it is a separate *API* question, and one switch for both was the
bug.  `rustdoc()` passes `--document-hidden-items` so the compiler still names
what it hides.

Running this check builds rustdoc JSON for the facade and for every workspace
crate whose re-exports it has to resolve.  Those builds go into a subdirectory
of their own under `CARGO_TARGET_DIR` -- see `GATE_TARGET` -- because every
`cargo doc` and `cargo rustdoc` in a checkout writes the same
`<target>/doc/<crate>.json`, and a document this gate did not ask for read as
one it did is an availability verdict about another feature set.  Pointed at a
cold subdirectory it is a build of the documents and their dependencies, which
is why CI gives it its own job; on an unchanged tree the rustdoc-JSON cache
answers without building at all.
"""

from __future__ import annotations

import argparse
import itertools
import json
from pathlib import Path
import re
import subprocess
import sys
import tomllib
from typing import Any, NamedTuple


SCRIPTS = Path(__file__).resolve().parent
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

from api_surface import (  # noqa: E402  (shared surface discovery)
    ApiItem,
    GATE_TARGET,
    TARGET,
    api_items,
    current_surface,
    doc_hidden,
    dump_surface,
    inventory_document,
    lash_core_surface,
    primary_path,
    rustdoc,
    rustdoc_json_cache,
    target_directory,
)

REPO = Path(__file__).resolve().parents[1]
INVENTORY = REPO / "docs" / "api-example-coverage.toml"
AREAS = {
    "sessions-turns",
    "processes",
    "triggers",
    "attachments",
    "stores",
    "plugins",
    "observation-admin",
    "protocol",
}
DISPOSITIONS = {
    "used-asserted",
    "used-unasserted",
    "unused-add",
    "unused-justify",
    "unused-remove",
    "internal-consumed",
    "internal-test-only",
}
#: Dispositions whose evidence is an internal seam rather than example coverage.
INTERNAL_DISPOSITIONS = {"internal-consumed", "internal-test-only"}
#: Dispositions that record an anchor at all, in the order the report prints.
ANCHORED_DISPOSITIONS = (
    "used-asserted",
    "used-unasserted",
    "internal-consumed",
    "internal-test-only",
)
#: Evidence tiers, strongest first.  See the module docstring.
ANCHOR_TIERS = ("example-host", "example-test", "crate-src", "workspace-tests")
#: The tiers each disposition's anchors may live in.  A row that blends them is
#: claiming a stronger kind of evidence than its anchor can carry.
DISPOSITION_TIERS = {
    "used-asserted": {"example-host", "example-test"},
    "used-unasserted": {"example-host", "example-test"},
    "internal-consumed": {"crate-src"},
    "internal-test-only": {"example-test", "workspace-tests"},
}
#: Rows whose usage anchor sits in an example's test code rather than its host
#: code.  A ratchet, not a target: the count may only fall, and lowering it is a
#: one-line diff beside the row that was upgraded.  Raising it is the bleeding
#: this gate exists to stop, so an increase fails.
#:
#: A raise is not forbidden, it is *argued*: the population may only grow
#: because a correctness fix admitted rows the old reading hid, and the PR body
#: has to state the delta and why the new number is the honest one, with the pin
#: moving in that same diff so a reviewer reads the raise rather than discovers
#: it.  PR #492 raised it 1914 -> 1959 on that footing.  Raising it because a
#: change would otherwise fail the gate is the thing this constant exists to
#: catch; the honest move there is to record the row's consumer in prose under
#: an `unused-*` disposition and leave the pin.
#:
#: FIG-1635 lowered it 1955 -> 1951: the trace execution map's four duplicate
#: identity fields (`module_ref`, `entry_kind`, `entry_ref`, `entry_name`) were
#: deleted from the public surface, taking their four example-test-anchored
#: rows with them.  A deletion lowering the pin is the ratchet tightening, not
#: bleeding.
EXAMPLE_TEST_TIER_RATCHET = 1922
#: Module paths that put an item behind the `testing` feature, which is what
#: makes a test-only consumer a home rather than an excuse.  `test_support` is
#: the doc-hidden one lash-core relocates cross-crate test-only items into;
#: `testing` is the documented fixture surface downstream crates share.
FEATURE_GATED_TEST_HOMES = ("::testing", "::test_support")
# AST-only Lashlang contracts are deliberately outside the host-facade walk,
# but additions made for FIG-1302 still require an explicit, checked
# disposition in the same registry.
REQUIRED_LOW_LEVEL_API = {
    "lashlang::CatchClause",
    "lashlang::DEFAULT_MAX_VM_FRAME_DEPTH",
    "lashlang::ErrorTaxonomy",
    "lashlang::InvalidAst",
    "lashlang::MAX_AST_NESTING_DEPTH",
    "lashlang::NestingTooDeep",
    "lashlang::RuntimeError::code",
    "lashlang::RuntimeError::taxonomy",
    "lashlang::VmFinallyCompletionContinuation",
    "lashlang::VmFinallyContinuation",
    "lashlang::VmHandlerContinuation",
    "lashlang::VmPendingErrorOriginContinuation",
    "lashlang::check_ast_nesting_depth",
    "lashlang::validate_ast",
    "lashlang::Declaration::Function",
    "lashlang::Expr::Call",
    "lashlang::Expr::Function",
    "lashlang::Expr::FunctionCall",
    "lashlang::FunctionDecl",
    "lashlang::FunctionParam",
    "lashlang::Expr::Map",
    "lashlang::Expr::Throw",
    "lashlang::Expr::Try",
    "lashlang::FunctionExpr",
    "lashlang::TryExpr",
    "lashlang::VmContinuation::frame_depth",
    "lashlang::compile_ast",
}


class InventoryTable(NamedTuple):
    """One inventory table's row shape.

    Both tables hold rows of the same shape and answer to the same validator:
    `row_errors` carries the whole body and both loops call it.  Where a check
    does not run, the reason is stated here rather than implied by which loop
    the row fell into -- the drift this declaration replaces is FIG-1865, where
    a hand-copied 26-line low-level loop silently skipped seventeen of the
    checks the `api` loop ran, six of them live on the standing rows.

    `absent_fields` names the fields a row of this table states no value for.
    A check that reads one of them has nothing to validate and is skipped on
    that ground alone.
    """

    name: str
    absent_fields: frozenset[str]

    def states(self, field: str) -> bool:
        """Whether a row of this table carries `field` at all."""
        return field not in self.absent_fields


API_TABLE = InventoryTable("api", frozenset())
# The two genuine differences between the tables, both stated rather than
# implied.  First, a low-level row identifies a Lashlang VM symbol, not a Rust
# surface item: it states no `kind`, `area`, `availability` or `aliases`, so
# the checks that read those fields -- and the internal-seam anchor checks,
# which resolve a member through its owning type's `kind` -- have nothing to
# read.  Second, the surface reconciliation each table answers to lives outside
# the row validator: `api` rows reconcile against `current_surface` in
# `item_errors`, low-level rows against `REQUIRED_LOW_LEVEL_API`.
LOW_LEVEL_TABLE = InventoryTable(
    "low_level_api", frozenset({"kind", "area", "availability", "aliases"})
)


def relocated_reference(reference: str) -> str | None:
    """Re-anchor a stale reference by exact source text within the same file.

    Returns the corrected reference, or None when the file is gone or no line
    matches the recorded source text — those need a human, not a refresh."""
    try:
        location, needle = reference.split("#", 1)
        relative, _ = location.rsplit(":", 1)
    except (ValueError, TypeError):
        return None
    path = REPO / relative
    if not any(
        path.is_relative_to(REPO / root) for root in ANCHOR_ROOTS
    ) or not path.is_file():
        return None
    lines = path.read_text(encoding="utf-8").splitlines()
    hits = [index + 1 for index, line in enumerate(lines) if needle in line]
    if not hits:
        return None
    return f"{relative}:{hits[0]}#{needle}"


def refresh() -> int:
    """Re-anchor every stale evidence reference whose source text still exists.

    Line numbers move whenever an example file is edited above an anchor; the
    recorded source text is the durable identity. Anything whose text is gone is
    reported and left untouched — deleting or rewriting evidence is a reviewed
    decision, not a refresh.

    Both inventory tables are covered. `low_level_api` rows anchor into the same
    example files and drift for the same reasons, so leaving them out made the
    one section that has to be maintained by hand the only one a refresh could
    not repair."""
    text = INVENTORY.read_text(encoding="utf-8")
    stale: list[str] = []
    replaced = 0
    with INVENTORY.open("rb") as handle:
        inventory = tomllib.load(handle)
    entries = [*inventory.get("api", []), *inventory.get("low_level_api", [])]
    # Several symbols legitimately cite the same line of an example, so
    # rewrites are collected before they are applied: rewriting per entry would
    # replace every occurrence on the first hit and then report the rest as
    # unrewritable.
    rewrites: dict[str, str] = {}
    for entry in entries:
        for field in ("usage", "assertion"):
            reference = entry.get(field, "")
            if not reference or reference_exists(reference):
                continue
            corrected = relocated_reference(reference)
            if corrected is None:
                stale.append(f"{entry.get('symbol')}: {field} evidence gone: {reference!r}")
                continue
            rewrites[reference] = corrected
    for reference, corrected in rewrites.items():
        escaped_old = toml_escape(reference)
        escaped_new = toml_escape(corrected)
        if escaped_old not in text:
            stale.append(f"could not rewrite reference {reference!r}")
            continue
        text = text.replace(escaped_old, escaped_new)
        replaced += 1
    INVENTORY.write_text(text, encoding="utf-8")
    print(f"refreshed {replaced} evidence anchors")
    for line in stale:
        print(f"- {line}", file=sys.stderr)
    return 1 if stale else 0


#: A token that names somewhere on one machine rather than in this repository.
#: `./x/y` is repository-relative, so a leading `.` must survive tokenizing.
MACHINE_LOCAL_ROOT = re.compile(r"^(?:/|~/|\\\\|[A-Za-z]:[\\/]|file://)")
#: Any wording that hands a consumer to the facade: migrate/move/port/switch to it.
MIGRATION_TO_FACADE = re.compile(
    r"\b(?:migrat|mov|port|switch|transition|relocat)\w*\b[^.]{0,60}?\bto\b[^.]{0,40}?"
    r"\b(?:fa[cç]ade|lash)\b",
    re.IGNORECASE,
)
#: Wording that denies the migration instead of promising it.
MIGRATION_DENIED = re.compile(
    r"\b(?:cannot|can ?not|can't|never|unable|impossible|no longer|not)\b", re.IGNORECASE
)
#: Assertions that hold for every non-ZST and therefore assert nothing.
TAUTOLOGICAL_ASSERTION = re.compile(r"\b(?:size_of|align_of)\b")
#: An assert invocation's opening line: the macro call, with every operand still
#: on the lines below it.  Nested opener macros (`assert!(matches!(`) count.
OPERANDLESS_ASSERTION = re.compile(
    r"^(?:debug_)?assert(?:_eq|_ne)?!\(\s*(?:[a-z_]+!\(\s*)*$"
)
#: Rust lines that only import a path into scope.
IMPORT_ONLY_EXERCISE = re.compile(r"^(?:pub(?:\([^)]*\))?\s+)?use\b")
#: Rust lines that only name or construct a type. These shapes can make a type
#: reachable to the compiler without observing any behavior of the value.
TYPE_ONLY_EXERCISE = re.compile(
    r"(?:\bfn\b[^\n{;]*(?:->|\([^)]*\))"
    r"|\blet\s+[^=;]+:\s*[^=;]+="
    r"|::[a-z_][A-Za-z0-9_]*\s*\("
    r"|(?:^|\W)[A-Z][A-Za-z0-9_]*(?:::[A-Za-z0-9_]+)*\s*\{)"
)
#: Associated functions whose API contract is construction rather than an
#: independently observable operation. The symbol, not merely the source line,
#: distinguishes `ModelSpec::builder(...)` from a behavioral associated call.
CONSTRUCTOR_FUNCTION = re.compile(
    r"^(?:new|default|builder|build|parse|opaque|text|stored|unit|bounded|unbounded"
    r"|any|create|with_.+|for_.+|child_.+|from(?:_.+)?|try_from(?:_.+)?)$"
)
#: A field binding inside a destructuring pattern, such as
#: `TurnEvent::Usage { usage, cumulative, .. }` split across lines.
DESTRUCTURED_FIELD = re.compile(r"^\s*(?:ref\s+|mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*,")
#: Assigning an associated function's result makes the value available to the
#: example but does not itself observe the function's contract. This catches
#: factory constructors without relying on naming conventions (`success`,
#: `failure`, and project-specific factories are constructors too).
ASSIGNED_RESULT = re.compile(
    r"\blet\s+(?:mut\s+)?[A-Za-z_][A-Za-z0-9_]*\b[^=]*="
)
#: Variant destructuring and equality guards identify a shape but do not
#: observe what handling that shape means to a host.
VARIANT_PATTERN_EXERCISE = re.compile(
    r"(?:\b(?:if|while)\s+let\b|\bmatches!\s*\(|=>|\{)"
)
#: Operand lines inside a closure or guarded match. They are useful assertion
#: anchors for the callback itself, but not for fluent setup elsewhere.
CLOSURE_ASSERTION_OPERAND = re.compile(r"\|[^|\n]*\|")
MATCH_GUARD_ASSERTION_OPERAND = re.compile(r"\bif\b")
#: The `unused-add` instruction template: "Add <path> to the <name> example ...".
ADD_INSTRUCTION_REASON = re.compile(
    r"^\s*Add\b[^.]*?\bto\b[^.]*?\bexample\b", re.IGNORECASE
)
#: The `used-unasserted` wording: no executed assertion reaches this item.
UNASSERTED_REASON = re.compile(
    r"compile-only|setup-only|reaches no executed assertion"
    r"|before claiming asserted usage",
    re.IGNORECASE,
)
#: Repository-relative source and documentation paths that reason prose may cite.
REPOSITORY_FILE_PATH = re.compile(
    r"^(?:\./)?(?:crates|examples|runbooks|scripts|docs)/"
    r"[^\s]+\.[A-Za-z][A-Za-z0-9]*(?::[0-9]+)?$"
)


def path_tokens(text: str) -> list[str]:
    """Path-shaped words in prose, stripped of quoting and sentence punctuation.

    A trailing `.` is sentence punctuation, but a leading one is part of a
    `./`-relative path, so the two ends strip differently.
    """
    tokens = []
    for word in re.split(r"[\s,;]+", text):
        token = word.lstrip("`'\"([{<“‘").rstrip("`'\"）)]}>”’.:,;")
        if token:
            tokens.append(token)
    return tokens


def quoted_prose(reason: str) -> str:
    """`reason` with every citation's quoted snippet cut back to its symbol.

    A snippet is source text, not prose, and the prose lints read it as prose:
    `"https://api.openai.com/v1/responses"` inside a quoted line is not a
    machine-local path, and a sentence a snippet happens to contain is not a
    claim the row is making.  Evidence anchors have always been read this way --
    only the location before `#` is a path -- and a citation is the same shape.
    """
    return PROSE_CITATION.sub(
        lambda match: (
            f"{citation_parts(match)[0]}::{citation_parts(match)[1]}"
            if citation_parts(match)[1]
            else citation_parts(match)[0]
        ),
        reason,
    )


def citation_file(token: str) -> str:
    """`token` with a symbol citation's `::symbol` path and `#snippet` tail cut.

    The file a citation names still has to exist; the symbol it names is checked
    by `prose_citation_defect`, which parses that file rather than reading prose.
    """
    token = token.split("#", 1)[0]
    match = re.match(r"^(.*\.[A-Za-z][A-Za-z0-9]*)::", token)
    return match.group(1) if match else token


def machine_local_path(text: str) -> str | None:
    """The first absolute or home-relative path in `text`, if any.

    Absolute evidence is unverifiable for everyone but its author: the path
    describes one checkout on one machine, and nothing in review or CI can tell
    whether it still says what it claimed.  Repository-relative evidence is
    checkable by anyone holding the repository.
    """
    for token in path_tokens(text):
        if not MACHINE_LOCAL_ROOT.match(token):
            continue
        if "/" in token[1:] or "\\" in token[1:]:
            return token
    return None


def missing_repository_path(text: str) -> str | None:
    """The first repository-relative file citation that no longer exists.

    Reason prose is durable evidence only while the file it points to remains
    in the repository.  Line anchors deliberately are not checked: moving code
    within a surviving file does not make the repository path itself false.
    """
    repository = REPO.resolve()
    for token in path_tokens(text):
        token = citation_file(token)
        if not REPOSITORY_FILE_PATH.match(token):
            continue
        relative = token.removeprefix("./")
        relative = re.sub(r":[0-9]+$", "", relative)
        path = (REPO / relative).resolve()
        if not path.is_relative_to(repository) or not path.is_file():
            return token
    return None


#: A `file.rs::Type::method` citation inside reason prose, optionally carrying a
#: verbatim ``#`snippet` `` for a target that is a statement rather than a symbol.
#: A backtick cannot appear in Rust code outside a comment or a string, so it is
#: the one delimiter a snippet can never have to escape.
PROSE_CITATION = re.compile(
    r"((?:crates|examples)/[^\s,;:()\"#`]+\.rs)"
    r"(?:::([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*))?"
    r"(?:#`([^`\n]+)`)"
    r"|((?:crates|examples)/[^\s,;:()\"#`]+\.rs)"
    r"::([A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*)"
)

#: The citation shape FIG-1550 retired: a line number pinning prose to a file.
#: Wider than the citation domain on purpose -- a `runbooks/` reference is not
#: resolvable against a symbol (the checker parses `crates/` and `examples/`),
#: but a line number on one is the same rot, and it may not come back either.
LINE_PINNED_CITATION = re.compile(
    r"((?:crates|examples|runbooks)/[^\s,;:()\"]+\.rs):([0-9]+)"
)

#: Prose naming the function a citation it follows sits in.
CITED_FUNCTION = re.compile(r"^,? (?:in|inside) `([A-Za-z0-9_]+)`")

#: Prose citing a snippet as evidence *against* itself: the anchor a row rejected.
COUNTER_CITATION = re.compile(r"^ asserts an unrelated expression in `([A-Za-z0-9_]+)`")

#: A snippet carrying no code: blank, punctuation alone, a comment, an attribute.
NON_CODE_LINE = re.compile(r"^(?:[\s{}();,\[\]]*|//.*|#!?\[.*)$")

#: Prose citing evidence the resolver cannot tie to the item, and saying so.
UNRESOLVED_CITATION = re.compile(
    r"^ as a consumer of this path and the checker cannot tie that citation to "
    r"the owning type mechanically"
)


def citation_parts(match: re.Match[str]) -> tuple[str, str | None, str | None]:
    """`(file, symbol path or None, snippet or None)` for a `PROSE_CITATION` hit."""
    if match.group(1) is not None:
        return match.group(1), match.group(2), match.group(3)
    return match.group(4), match.group(5), None

#: A plain string literal, escapes honoured, newlines allowed.
CODE_LITERAL = re.compile(r"b?\"(?:\\.|[^\"\\])*\"", re.S)

#: The opening of a raw string; its close is the same hash run, found by hand.
RAW_LITERAL_OPEN = re.compile(r"b?r(#*)\"")

#: A literal holding one bare identifier and nothing else -- a wire key, not a
#: sentence.  Dots and dashes are excluded on purpose: `"process.abandoned"`
#: is a *different* item's wire name, and letting it through licenses one
#: symbol's string as evidence for another's field.
NAME_LITERAL = re.compile(r"^b?r?#*\"[A-Za-z_][A-Za-z0-9_]*\"#*$")

#: The name a `let` introduces, which the surrounding code chose freely.
LET_BINDER = re.compile(r"\blet\s+(?:mut\s+)?([A-Za-z_][A-Za-z0-9_]*)")


def consuming_text(text: str) -> str:
    """`text` with the places a name can appear without being a use blanked out.

    A leaf name is a common English word -- `deferred`, `abort`, `cancellation` --
    and two places spell one without touching the item.  A sentence in a string
    is prose: `.expect("a deferred digest keeps its bytes")` names nothing, and
    51 citations rested on exactly that.  A `let` binder is the *caller's* choice
    of name: `let mut abort = None;` is a local the author could have called
    anything, and reading it as evidence for `TurnPreparation::abort` reverses
    who is proving what.

    A literal holding a single name survives, because that is how the wire is
    asserted: `assert_eq!(projected["external_ref"]["id"], ...)` observes the
    field this row is about, and blanking it would delete the evidence the
    serialization rows rest on.  Every other spelling stays too: a field read, an
    argument, a struct-literal key, a method call, a type.

    Comments go the same way, and they are why this reads the text one character
    at a time instead of running a regex over it: an apostrophe or a lone quote
    in a sentence is not the start of a literal, and a pattern that thinks it is
    swallows every line up to the next quote -- real code included.
    """
    kept: list[str] = []
    index, length = 0, len(text)
    while index < length:
        if text.startswith("//", index):
            newline = text.find("\n", index)
            index = length if newline == -1 else newline
            continue
        if text.startswith("/*", index):
            depth, scan = 1, index + 2
            while scan < length and depth:
                if text.startswith("/*", scan):
                    depth, scan = depth + 1, scan + 2
                elif text.startswith("*/", scan):
                    depth, scan = depth - 1, scan + 2
                else:
                    # A literal or comment may span lines, and dropping its
                    # newlines would slide every line after it.
                    kept.append("\n" if text[scan] == "\n" else "")
                    scan += 1
            index = scan
            continue
        raw = RAW_LITERAL_OPEN.match(text, index)
        if raw:
            close = text.find('"' + raw.group(1), raw.end())
            end = length if close == -1 else close + 1 + len(raw.group(1))
            body = text[index:end]
        else:
            plain = CODE_LITERAL.match(text, index)
            body = plain.group(0) if plain else ""
            end = plain.end() if plain else index
        if body:
            kept.append(body if NAME_LITERAL.match(body) else '""' + "\n" * body.count("\n"))
            index = end
            continue
        kept.append(text[index])
        index += 1
    without_literals = "".join(kept)
    return LET_BINDER.sub(
        lambda match: match.group(0)[: match.start(1) - match.start(0)] + "_local",
        without_literals,
    )


def consuming_file_lines(relative: str) -> list[str]:
    """A file's lines with literals and `let` binders blanked, cached per file.

    Masked whole-file rather than line by line: a message that runs over three
    lines only looks like a string from above, and read one line at a time its
    middle reads as code.
    """
    if relative not in _CONSUMING_LINES:
        lines = source_file_lines(relative) or []
        _CONSUMING_LINES[relative] = consuming_text("\n".join(lines)).split("\n")
    return _CONSUMING_LINES[relative]


def prose_citations(entries: list[dict[str, Any]]) -> int:
    """How many symbol citations the reasons across the ledger carry."""
    return sum(
        len(PROSE_CITATION.findall(entry.get("reason", "") or "")) for entry in entries
    )


def anchor_locations(entry: Any) -> set[tuple[str, int]]:
    """The `(file, line)` pairs a row records as its own evidence anchors."""
    located = set()
    for field in ("usage", "assertion"):
        head = str(entry.get(field, "") or "").split("#", 1)[0]
        relative, _, line_text = head.rpartition(":")
        if relative and line_text.isdigit():
            located.add((relative, int(line_text)))
    return located


def prose_citation_defect(
    symbol: str,
    kind: str,
    reason: str,
    anchors: set[tuple[str, int]] | None = None,
) -> str | None:
    """Why a cited symbol does not show what the prose says it shows, if so.

    Prose is only evidence while the code it points at is about this item.  The
    rows this arc first wrote cited real files at real lines that never mentioned
    the symbol -- 64 of them -- which reads as verification and is not.  The check
    is deliberately the anchor rule's weaker sibling: the item's own name, or the
    type that owns it, has to appear in the symbol the citation names.

    A citation names a *symbol*, never a line (FIG-1550).  `file.rs::Type::method`
    is resolved by parsing the file at gate time, and a citation whose target is a
    statement rather than a whole symbol carries the enclosing symbol plus a
    verbatim ``#`snippet` `` matched inside its span.  Line pins broke on every
    rebase that touched an anchored file while rotting silently whenever a shift
    happened to land on code -- 94 citations decayed in one wave, and the seven
    `--refresh` mis-relocations that wave were repairs to breakage that carried no
    information.  A symbol citation breaks when the symbol moves file or dies, and
    a snippet citation additionally when its text leaves the symbol: both are
    exactly the moment a human should re-read the claim.

    More than one span may answer to one symbol path -- two impl blocks can carry
    the same method name -- and that is not a defect: the symbol is the anchor and
    the snippet is the drift detector, so a snippet found in any of them is found.
    Zero spans, or a snippet in none of them, is the loud break.

    Scope: every citation in every reason.  FIG-1223 first scoped the check to
    internal dispositions and rows whose prose invoked FIG-1223, grandfathering
    298 older citations; that scope was escapable by simply not naming the ticket,
    so FIG-1526 re-anchored the grandfathered citations and removed the scope.

    Prose says more than *where*, and the rest is checkable too.  A reason that
    says a citation sits `in `some_function`` is read against the symbol path,
    because a citation whose surrounding prose names a function the path does not
    contain reads as a located fact and is a guess.  A reason that cites evidence
    to *reject* it -- the anchor a downgraded row refuses to call evidence -- is
    held to the inverse: it has to quote an assertion, in the function the prose
    names, whose symbol says nothing about this item.  Without that inverse the
    only way to keep such a row honest is to drop the citation, which is how 24
    rows went invisible in the first FIG-1526 round.

    A snippet lands on code.  588 citations once landed on a blank line, a lone
    brace, a comment or an attribute and read as located facts, because the file
    or the function elsewhere happened to name the item.

    A row that says outright that the resolver cannot tie its citation to the
    item -- an extension trait reached only through a `use` and a method call --
    is held to the weakest honest claim there is: the citation has to spell the
    item, in its snippet where it has one.  A brace or an attribute cannot, which
    is the whole difference between recording a candidate and manufacturing one.

    Naming is read over `consuming_text`, so a leaf spelled inside a string
    literal or introduced by a `let` is not a use of the item: `.expect("a
    deferred digest keeps its bytes")` and `let mut abort = None;` are the two
    shapes that let a common English word stand in for evidence.

    A citation to the symbol holding the row's own recorded anchor is exempt from
    the naming rule and from nothing else: `reference_exists` re-reads that exact
    line and its quoted source on every run, and the anchor rules above decide
    whether it is evidence.  Prose repeating a location the ledger already proves
    is not a second, weaker claim -- and an assertion in a test whose function
    never spells the item is exactly the anchor an example is entitled to record.
    """
    leaf = symbol.split("::")[-1]
    owner = symbol.split("::")[-2] if "::" in symbol else ""
    pinned = LINE_PINNED_CITATION.search(reason)
    if pinned:
        return (
            f"pins prose to {pinned.group(0)} by line number; citations name a "
            "symbol -- `file.rs::Type::method`, with a verbatim `#`snippet`` when "
            "the target is a statement inside it (FIG-1550)"
        )
    for match in PROSE_CITATION.finditer(reason):
        relative, path, snippet = citation_parts(match)
        cited = f"{relative}::{path}" if path else relative
        lines = source_file_lines(relative)
        if lines is None:
            return f"cites {cited}, which is not a source file in this repository"
        if path is None:
            spans = [((0, len(lines) - 1),)]
        else:
            spans = symbol_spans(relative, path)
            if not spans:
                return (
                    f"cites {cited}, but that file declares no such symbol -- it "
                    "moved to another file or was deleted"
                )
        if snippet is not None:
            if NON_CODE_LINE.match(snippet.strip()):
                return (
                    f"cites `{snippet}` in {cited}, which carries no code -- a "
                    "citation lands on the code it is about, not near it"
                )
            found = [
                span for span in spans if snippet in symbol_span_text(relative, span)
            ]
            if not found:
                return (
                    f"cites `{snippet}` in {cited}, but that text does not appear "
                    "there -- the code it quoted has changed or moved away"
                )
            spans = found
        texts = [consuming_text(symbol_span_text(relative, span)) for span in spans]
        tail = reason[match.end() :]
        names_item = any(
            names_word(leaf, text) or bool(owner and names_word(owner, text))
            for text in texts
        )
        # A snippet that survives masking inside its span is code as written; one
        # that does not is read through `consuming_text` on its own, which is what
        # separates `assert_eq!(projected["deferred"], 1)` -- a wire key observing
        # the field -- from `let mut deferred = None;`, a local the author named.
        code = snippet
        if snippet is not None and not any(snippet in text for text in texts):
            code = consuming_text(snippet)
        spelled = snippet is not None and (
            names_word(leaf, snippet) or bool(owner and names_word(owner, snippet))
        )
        spells_item = code is not None and (
            names_word(leaf, code) or bool(owner and names_word(owner, code))
        )
        if spelled and not spells_item:
            return (
                f"cites `{snippet}` in {cited}, where {leaf} is a local name or "
                "literal text rather than a use of the item"
            )
        if path is None and not spells_item:
            # A citation with no symbol to resolve has the whole file as its
            # window, and a file that names the item *somewhere* would answer for
            # a snippet about anything -- which is the shape FIG-1526 removed when
            # it stopped reading the file as a line's scope.  So the snippet
            # carries the whole claim: the 18 rows in this form are `use` lines
            # that spell the item they import.
            return (
                f"cites `{snippet}` at file scope in {cited}, where neither {leaf} "
                "nor the type that owns it appears -- a citation naming no symbol "
                "is confirmed by its snippet or not at all"
            )
        masked_lines = consuming_file_lines(relative)
        declares_item = any(
            names_word(leaf, masked_lines[span[-1][0]])
            or bool(owner and names_word(owner, masked_lines[span[-1][0]]))
            for span in spans
            if span[-1][0] < len(masked_lines)
        )
        own_anchor = any(
            relative == anchored and any(span[-1][0] <= line - 1 <= span[-1][1] for span in spans)
            for anchored, line in (anchors or set())
        )
        rejected = COUNTER_CITATION.match(tail)
        if rejected:
            if snippet is None or "assert" not in snippet:
                return (
                    f"cites {cited} as the assertion it rejects, but quotes no "
                    "asserting snippet"
                )
            if names_item:
                return (
                    f"calls {cited} unrelated to {leaf}, but that symbol names the "
                    "item, so the rejection is wrong"
                )
        elif UNRESOLVED_CITATION.match(tail):
            # The weakest honest claim there is, and it is held to the narrowest
            # window: the snippet where the citation quotes one, and the symbol's
            # own declaration line where it does not.  Reading the whole span here
            # would let a row whose resolver gave up point at any function that
            # happens to mention the item.
            if not (spells_item if snippet is not None else declares_item):
                return (
                    f"records {cited} as the consumer candidate the resolver cannot "
                    f"tie to {leaf}, but the citation does not name it"
                )
        elif not names_item and not own_anchor:
            return (
                f"cites {cited}, where neither {leaf} nor the type that owns it "
                "appears anywhere in the symbol"
            )
        named = rejected or CITED_FUNCTION.match(tail)
        if named and named.group(1) not in (path or "").split("::"):
            return (
                f"places {cited} in `{named.group(1)}`, but that name is not part "
                "of the symbol the citation resolves"
            )
    return None


def tautological_assertion(assertion: str) -> bool:
    """Whether an assertion anchor asserts something that is always true.

    `assert!(size_of::<T>() > 0)` holds for every non-ZST, so it witnesses that
    the path compiles and nothing about whether a host exercised the item.  An
    item resting on one is undispositioned, not covered.
    """
    return bool(TAUTOLOGICAL_ASSERTION.search(assertion.split("#", 1)[-1]))


def uninformative_assertion(assertion: str, source: str) -> str | None:
    """Why an assertion anchor states nothing observable, if so.

    An anchor is evidence because a reader can open the line and see what the
    example observed.  Two shapes defeat that.  The first is the opening line of
    a multi-line assertion: `assert_eq!(` names a macro and no operands, so it
    reads as proof while asserting nothing in particular -- and it is
    indistinguishable, in the inventory, between a real outcome check and a
    coincidence.  The second is an anchor that quotes only the start of its line,
    which hides the rest of the assertion; `assert!(saw_gap,` drops the very
    message that said what the gap meant.  Both are why FIG-970 re-anchored 361
    rows to the line that carries the observation.
    """
    quoted = assertion.split("#", 1)[-1].strip()
    line = source.strip()
    if OPERANDLESS_ASSERTION.match(quoted):
        return (
            "quotes only the opening line of a multi-line assertion, so it "
            "carries no operands and observes nothing"
        )
    if line and quoted != line:
        return f"quotes part of its line, which reads {line!r}"
    return None


def perfunctory_exercise(
    symbol: str,
    kind: str,
    source: str,
    disposition: str = "",
) -> str | None:
    """Why a usage line proves syntax reachability rather than exercise."""
    line = source.strip()
    explicitly_unasserted = disposition == "used-unasserted"
    # A direct assertion is an observation even when one operand constructs a
    # comparison value or names a variant. The rejected shape is using that
    # syntax as the usage anchor and borrowing some other assertion later.
    if re.match(r"^(?:debug_)?assert(?:_eq|_ne)?!\s*\(", line):
        return None
    if IMPORT_ONLY_EXERCISE.match(line):
        if explicitly_unasserted:
            return None
        return "is an import, which only brings the item into scope"
    variant_shape = VARIANT_PATTERN_EXERCISE.search(line) or re.search(
        r"::[A-Z][A-Za-z0-9_]*\b", line
    )
    if kind in {"enum", "variant"} and variant_shape:
        if explicitly_unasserted:
            return None
        return "is a variant pattern, which only distinguishes an enum shape"
    if kind == "field" and DESTRUCTURED_FIELD.match(line):
        if explicitly_unasserted:
            return None
        return "is a variant pattern, which only destructures a field"
    symbol_leaf = symbol.rsplit("::", 1)[-1]
    names_function = re.search(rf"\b{re.escape(symbol_leaf)}\s*\(", line)
    constructor_function = kind == "function" and (
        CONSTRUCTOR_FUNCTION.match(symbol_leaf)
        or (ASSIGNED_RESULT.search(line) and names_function)
    )
    if (
        kind in {"enum", "struct", "trait", "type_alias", "union"}
        and TYPE_ONLY_EXERCISE.search(line)
    ) or (constructor_function and names_function):
        if explicitly_unasserted:
            return None
        return "only constructs or declares the type, without observing its behavior"
    return None


def unrelated_fluent_assertion(usage_source: str, assertion_source: str) -> str | None:
    """Why a fluent call's assertion line belongs to a callback instead."""
    if not usage_source.strip().startswith("."):
        return None
    assertion = assertion_source.strip()
    if CLOSURE_ASSERTION_OPERAND.search(assertion):
        return "inherits a closure operand that can observe an unrelated callback"
    if MATCH_GUARD_ASSERTION_OPERAND.search(assertion):
        return "inherits a match guard that can observe an unrelated callback"
    return None


def stale_disposition_reason(disposition: str, reason: str) -> str | None:
    """Why a reason describes a disposition this row does not hold, if so.

    A reason is the row's own account of its contract, so a reason left over from
    a different verdict is a false statement, not untidiness.  Both shapes below
    shipped in the FIG-955 inventory: 904 rows with real usage and assertion
    anchors still carried the `unused-add` instruction to add them to an example,
    and 121 `used-asserted` rows still carried the `used-unasserted` wording that
    says no executed assertion reaches them.
    """
    if not reason.strip():
        return None
    if disposition.startswith("used-") and ADD_INSTRUCTION_REASON.search(reason):
        return (
            "is the unused-add instruction to add this item to an example, but "
            "the row records real usage evidence; describe what the recorded "
            "anchors actually exercise"
        )
    if disposition == "used-asserted" and UNASSERTED_REASON.search(reason):
        return (
            "says no executed assertion reaches this item, but the row records "
            "an assertion anchor; one of the two is wrong"
        )
    return None


_METADATA: dict[str, Any] = {}


def cargo_metadata() -> dict[str, Any]:
    """The resolved workspace graph, read once per run.

    All features, so a feature-gated dependency edge cannot hide a cycle from
    `facade_dependency_dirs` or a crate from `crate_directories`.
    """
    if not _METADATA:
        completed = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--all-features", "--locked"],
            cwd=REPO,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        )
        _METADATA.update(json.loads(completed.stdout))
    return _METADATA


def crate_directories() -> dict[str, str]:
    """`{crate identifier: directory}` for every workspace library crate.

    An `internal-consumed` anchor has to name a *consuming* crate, so the check
    needs the defining crate's directory: the identity root `lash_core` is the
    `crates/lash-core` library, and an anchor there proves only that the crate
    uses its own code.  Derived from the resolved graph rather than from the
    directory name, because a package name and its library name need not match --
    `lash-runtime` lives in `crates/lash`.
    """
    metadata = cargo_metadata()
    members = set(metadata["workspace_members"])
    directories: dict[str, str] = {}
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        manifest = Path(package["manifest_path"]).parent
        if not manifest.is_relative_to(REPO):
            continue
        directory = manifest.relative_to(REPO).as_posix()
        for target in package["targets"]:
            if "lib" in target["kind"] or "proc-macro" in target["kind"]:
                directories[target["name"].replace("-", "_")] = directory
    return directories


def facade_dependency_dirs() -> set[str]:
    """Workspace crate directories the `lash` facade is built on.

    A crate in this set cannot depend on the facade -- that would be a cycle --
    so a consumer living there can never "migrate to the lash facade".  Derived
    from the resolved dependency graph so the rule cannot drift out of date the
    way a hand-written crate list would.
    """
    metadata = cargo_metadata()
    members = set(metadata["workspace_members"])
    packages = {package["id"]: package for package in metadata["packages"]}
    dependencies = {
        node["id"]: [dependency["pkg"] for dependency in node["deps"]]
        for node in metadata["resolve"]["nodes"]
    }
    facade = next(
        package_id
        for package_id, package in packages.items()
        if package["name"] == "lash-runtime"
    )
    reached: set[str] = set()
    frontier = [facade]
    while frontier:
        current = frontier.pop()
        for dependency in dependencies.get(current, []):
            if dependency in reached:
                continue
            reached.add(dependency)
            frontier.append(dependency)
    directories = set()
    for package_id in reached & members:
        manifest = Path(packages[package_id]["manifest_path"]).parent
        if manifest.is_relative_to(REPO):
            directories.add(manifest.relative_to(REPO).as_posix())
    return directories


def impossible_facade_migration(reason: str, facade_dirs: set[str]) -> str | None:
    """The cited consumer path that cannot migrate to the facade, if any.

    `lash-core` is the loud case: it defines the types the facade re-exports, so
    a justification saying a `crates/lash-core/...` caller will move to `lash`
    describes a dependency cycle.  The seam is real; only the migration story is
    fiction -- which is why the check is on the promise, not on the phrase.

    Two properties keep it honest.  It reads sentence by sentence, so the crate
    blamed is the one the claim is about rather than any path mentioned anywhere
    in the prose.  And a denial is not a claim: "that caller *cannot* migrate to
    the `lash` facade" states the very cycle this rejects, so rejecting it would
    punish the correct description.
    """
    for sentence in re.split(r"(?<=[.])\s+|\n", reason):
        claim = MIGRATION_TO_FACADE.search(sentence)
        if claim is None:
            continue
        if MIGRATION_DENIED.search(sentence[: claim.end()]):
            continue
        for token in path_tokens(sentence):
            location = token.rsplit(":", 1)[0] if ":" in token else token
            for directory in facade_dirs:
                if location == directory or location.startswith(f"{directory}/"):
                    return token
    return None


def toml_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


#: Roots an evidence anchor may point into.  `examples/` is host coverage;
#: `crates/` is where an internal seam's consumer lives.
ANCHOR_ROOTS = ("examples", "crates")
#: The `cfg` predicate of an attribute, however the predicate is spelled.
CFG_ATTRIBUTE = re.compile(r"#\[cfg\(")
#: A char literal, the only shape in which `'` opens one: `'a'`, `'\n'`.  A
#: lifetime is not one, and reading it as one swallows the rest of the line.
CHAR_LITERAL = re.compile(r"'(?:\\.|[^'\\])'")
#: An attribute or comment line: whatever it holds, it never ends the item a
#: pending `cfg` gate is waiting for.
CFG_GATE_CONTINUATION = re.compile(r"^\s*(?:#!?\[|//|/\*|\*)")
#: How many free cfg atoms a predicate may carry before the evaluator stops
#: enumerating assignments.  Real predicates carry two or three; past this bound
#: the predicate is read as shipped rather than guessed at.  That is the safer
#: answer for an internal seam -- a row may not claim test-only without the
#: enumeration having shown it -- and the weaker one for the example tiers,
#: where reading an example's test code as host code would let the tier ratchet
#: fall without the row being upgraded.  The bound is set where no predicate in
#: this workspace comes near it, so the trade is theoretical; move it rather
#: than let a real predicate reach it.
CFG_ATOM_LIMIT = 12
#: A module declared without a body: the code is in another file.
OUT_OF_LINE_MODULE = re.compile(
    r"(?:pub(?:\s*\([^)]*\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;"
)
#: `#[path = "other.rs"]`, which decides where a module's code actually lives.
PATH_ATTRIBUTE = re.compile(r"#\[path\s*=\s*\"([^\"]+)\"\s*\]")
_SOURCE_LINES: dict[str, list[str] | None] = {}
_CONSUMING_LINES: dict[str, list[str]] = {}
_SCOPE_BLOCKS: dict[str, list[tuple[int, int, str]]] = {}
_SYMBOL_BLOCKS: dict[str, list[tuple[int, int, str]]] = {}
_IMPORTED_TYPES: dict[str, set[str]] = {}
_TEST_REGIONS: dict[str, list[tuple[int, int]] | None] = {}
_TEST_MODULES: tuple[frozenset[str], tuple[str, ...]] | None = None


def anchor_location(reference: str) -> tuple[str, int] | None:
    """The `path`, `line` an anchor names, or None when it is not an anchor."""
    try:
        location, _ = reference.split("#", 1)
        relative, line_text = location.rsplit(":", 1)
        line_number = int(line_text)
    except (ValueError, TypeError, AttributeError):
        return None
    if line_number < 1:
        return None
    return relative, line_number


def balanced_group(text: str, start: int) -> str:
    """The contents of the parenthesis group opening at `start`."""
    depth = 0
    for index in range(start, len(text)):
        if text[index] == "(":
            depth += 1
        elif text[index] == ")":
            depth -= 1
            if depth == 0:
                return text[start + 1 : index]
    return text[start + 1 :]


def split_predicates(text: str) -> list[str]:
    """The comma-separated predicates of a `cfg` list, respecting nesting."""
    parts: list[str] = []
    depth = 0
    quoted = False
    current: list[str] = []
    index = 0
    while index < len(text):
        character = text[index]
        if quoted:
            if character == "\\":
                current.append(text[index : index + 2])
                index += 2
                continue
            if character == '"':
                quoted = False
        elif character == '"':
            quoted = True
        elif character == "(":
            depth += 1
        elif character == ")":
            depth -= 1
        elif character == "," and depth == 0:
            parts.append("".join(current))
            current = []
            index += 1
            continue
        current.append(character)
        index += 1
    parts.append("".join(current))
    return [part.strip() for part in parts if part.strip()]


def parse_cfg(text: str) -> tuple[str, object]:
    """A `cfg` predicate as a tree of `("all"|"any"|"not"|"atom", payload)`.

    The operand of an `all`/`any`/`not` is a list of subtrees; the payload of an
    atom is the name a build turns on, so `feature = "testing"` and `unix` are
    both just names to assign.  An atom's spacing is not part of its identity --
    `feature="x"` and `feature = "x"` name one feature -- so the name is
    normalised rather than left to two spellings that would count as two atoms.
    """
    text = text.strip()
    for operator in ("all", "any", "not"):
        if text.startswith(operator):
            rest = text[len(operator) :].lstrip()
            if rest.startswith("("):
                inner = balanced_group(rest, 0)
                return operator, [parse_cfg(part) for part in split_predicates(inner)]
    return "atom", re.sub(r"\s*=\s*", " = ", " ".join(text.split()))


def cfg_atoms(tree: tuple[str, object]) -> set[str]:
    """Every atom name a predicate tree mentions."""
    operator, payload = tree
    if operator == "atom":
        return {str(payload)}
    names: set[str] = set()
    for child in payload:  # type: ignore[union-attr]
        names |= cfg_atoms(child)  # type: ignore[arg-type]
    return names


def evaluate_cfg(tree: tuple[str, object], enabled: set[str]) -> bool:
    """Whether a predicate holds for a build that turned on exactly `enabled`.

    `not` takes exactly one predicate in Rust, and the assert says so rather
    than letting a malformed `not(a, b)` quietly read as "not both".
    """
    operator, payload = tree
    if operator == "atom":
        return str(payload) in enabled
    children = [
        evaluate_cfg(child, enabled)  # type: ignore[arg-type]
        for child in payload  # type: ignore[union-attr]
    ]
    if operator == "all":
        return all(children)
    if operator == "any":
        return any(children)
    assert len(children) == 1, f"cfg not() takes one predicate, got {len(children)}"
    return not children[0]


def cfg_gates_test(line: str) -> bool:
    """Whether a `cfg` attribute on this line compiles the item for tests only.

    The question a tier has to answer is not "does this predicate mention
    `test`" but "can this item reach a shipped build" -- and the answer needs the
    predicate evaluated, because `test` reads differently under every operator.
    `all(test, feature = "testing")` never ships; `any(test, feature =
    "core-conversions")` ships whenever that feature is on, and reading it as
    test code filed a whole directory of shipped conversions as tests
    (FIG-1533).  `not(test)` inverts again, and nesting composes all three.

    So: pin `test` off, leave every other atom free -- features a downstream
    build may turn on, platform predicates a target may satisfy -- and ask
    whether any assignment still compiles the item.  If one does, the item
    ships.  Only when none does is the item test-only.
    """
    match = CFG_ATTRIBUTE.search(line)
    if match is None:
        return False
    tree = parse_cfg(balanced_group(line, match.end() - 1))
    free = sorted(cfg_atoms(tree) - {"test"})
    if len(free) > CFG_ATOM_LIMIT:
        return False
    for assignment in itertools.product((False, True), repeat=len(free)):
        enabled = {name for name, on in zip(free, assignment) if on}
        if evaluate_cfg(tree, enabled):
            return False
    return True


def gate_release_text(line: str) -> str:
    """`line` with comments and literals blanked, for finding where an item ends.

    The brace or semicolon that ends a gated item has to be the code's, not a
    sentence's: a doc comment between the gate and its item -- `/// Enabled;
    tests only.` -- would otherwise end the item at a word, and a `"{"` in a
    string would open a body that is not there.  Lifetimes keep their quote: `'`
    starts a literal only in the two shapes a char literal has.

    Only `//` is blanked here; a `/* .. */` comment is handled a line at a time
    by `CFG_GATE_CONTINUATION` instead, which reads a line opening or continuing
    a block comment as not-yet-the-item and leaves the gate pending.  The
    continuation therefore covers the common shape -- a block comment on its own
    lines between gate and item -- and not the rare one where code shares the
    line that closes the comment, which no `cfg`-gated item here writes.
    """
    kept: list[str] = []
    index, length = 0, len(line)
    while index < length:
        if line.startswith("//", index):
            break
        character = line[index]
        if character == '"':
            index += 1
            while index < length:
                if line[index] == "\\":
                    index += 2
                    continue
                if line[index] == '"':
                    index += 1
                    break
                index += 1
            kept.append(" ")
            continue
        literal = CHAR_LITERAL.match(line, index)
        if literal is not None:
            kept.append(" ")
            index = literal.end()
            continue
        kept.append(character)
        index += 1
    return "".join(kept)


def after_cfg_attribute(line: str) -> str:
    """What a line says after its `cfg` attribute closes.

    A gate and its item share a line often enough that the item's own shape --
    a body, or a semicolon -- has to be read from the remainder rather than
    from the whole line.
    """
    match = CFG_ATTRIBUTE.search(line)
    if match is None:
        return line
    start = match.end() - 1
    depth = 0
    for index in range(start, len(line)):
        if line[index] == "(":
            depth += 1
        elif line[index] == ")":
            depth -= 1
            if depth == 0:
                closing = line.find("]", index)
                return line[closing + 1 :] if closing != -1 else ""
    return ""


def test_regions(lines: list[str]) -> list[tuple[int, int]]:
    """Line ranges of `#[cfg(test)]` items in a source file.

    Brace depth, not indentation: a test module's contents can be nested
    arbitrarily and a file can hold several of them.  This is what separates an
    example's host code from the tests beside it, and the separation is the whole
    point of the tier -- 84% of this inventory's "exercised by an example"
    evidence turned out to be an example's own tests (FIG-1223).

    A gate belongs to exactly one item, so it has to be released by the item it
    was written for.  A bodyless `#[cfg(test)] mod support;` ends at its
    semicolon; leaving the gate pending handed it to the next braced item in
    the file, which is how an unrelated shipped module read as a test region
    and swallowed a real gate further down (FIG-1533).

    Released is not discarded.  The statement between the gate and that
    semicolon is itself test code -- `#[cfg(test)]\\nself.hook();` spans two
    lines and both of them are gated -- so the release closes a region over the
    item rather than dropping it, which is the difference between ending a
    region and never having one.
    """
    regions: list[list[int]] = []
    depth = 0
    gate: int | None = None
    for number, line in enumerate(lines, start=1):
        tail = line
        if gate is None and cfg_gates_test(line.strip()):
            gate = number
            gate_depth = depth
            tail = after_cfg_attribute(line)
        opened = line.count("{")
        after = depth + opened - line.count("}")
        # An attribute or comment under the gate is not the gated item: only the
        # gate's own line is read past its attribute, and the rest wait for code.
        pending = gate is not None and (gate == number or not CFG_GATE_CONTINUATION.match(line))
        if pending:
            release = gate_release_text(tail)
            brace = release.find("{")
            semicolon = release.find(";")
            if brace != -1 and (semicolon == -1 or brace < semicolon):
                regions.append([gate, 0, gate_depth])
                gate = None
            elif semicolon != -1:
                # Known case, named rather than guarded: a braced signature can
                # carry a `;` ahead of its `{` -- `fn f(b: [u8; 4]) {` -- and
                # this branch then closes the region at the signature and reads
                # the body as shipped code. Positions are compared, so only a
                # semicolon *before* the brace does it, and no `cfg`-gated item
                # in the workspace has one; parsing types to rule it out would
                # cost more than the case is worth. Revisit if one appears.
                regions.append([gate, number, gate_depth])
                gate = None
        for region in regions:
            if not region[1] and after <= region[2]:
                region[1] = number
        depth = after
    return [(start, end or len(lines)) for start, end, _ in regions]


def cfg_test_regions(relative: str) -> list[tuple[int, int]] | None:
    """`test_regions` for a repository file, or None when it cannot be read."""
    if relative not in _TEST_REGIONS:
        lines = source_file_lines(relative)
        _TEST_REGIONS[relative] = None if lines is None else test_regions(lines)
    return _TEST_REGIONS[relative]


def module_directory(relative: str) -> str:
    """Where an out-of-line submodule of this file lives.

    `src/a/mod.rs` and `src/a.rs` both own `src/a/`, and a crate root owns its
    own directory.
    """
    parent, _, name = relative.rpartition("/")
    stem = name.removesuffix(".rs")
    if stem in {"mod", "lib", "main"}:
        return parent
    return f"{parent}/{stem}" if parent else stem


def declared_test_modules(lines: list[str]) -> list[tuple[str, str | None]]:
    """`(name, path)` for every out-of-line module this file declares for tests.

    `#[cfg(test)] mod support;` compiles a whole *other* file as test code, and
    that file carries no marker of its own: read on its own it is
    indistinguishable from shipped source.  92 files in this repository are test
    code only by their declaration, and reading them as `crate-src` is what let
    test-only evidence justify internal seams (FIG-1223).

    Three spellings all count, because the compiler treats them alike: the gate
    and the declaration on separate lines, on the same line, and a `#[path]`
    attribute that sends the module somewhere the module name does not predict.
    """
    declared: list[tuple[str, str | None]] = []
    gated = False
    path: str | None = None
    for line in lines:
        stripped = line.strip()
        if not stripped or stripped.startswith(("//", "/*", "*")):
            continue
        if cfg_gates_test(stripped):
            gated = True
        override = PATH_ATTRIBUTE.search(stripped)
        if override:
            path = override.group(1)
        if gated:
            declaration = OUT_OF_LINE_MODULE.search(stripped)
            if declaration:
                declared.append((declaration.group(1), path))
                gated = False
                path = None
                continue
            if not stripped.startswith("#["):
                gated = False
                path = None
        elif not stripped.startswith("#["):
            path = None
    return declared


def test_module_paths() -> tuple[frozenset[str], tuple[str, ...]]:
    """Files, and directory prefixes, that are test code by declaration.

    Scanned once for the whole repository: the fact lives in the *parent* file,
    so no per-anchor lookup can find it.  A module's directory comes along with
    it, because a test module's submodules are test code too.
    """
    global _TEST_MODULES
    if _TEST_MODULES is None:
        files: set[str] = set()
        directories: set[str] = set()
        for root in ANCHOR_ROOTS:
            for path in sorted((REPO / root).rglob("*.rs")):
                relative = path.relative_to(REPO).as_posix()
                lines = source_file_lines(relative)
                if lines is None:
                    continue
                for name, override in declared_test_modules(lines):
                    if override is not None:
                        # `#[path]` is relative to the declaring file's own
                        # directory, and it is the whole point of the attribute
                        # that the name predicts nothing about the location.
                        directory = relative.rpartition("/")[0]
                        target = f"{directory}/{override}".replace("/./", "/")
                        files.add(target)
                        directories.add(f"{target.removesuffix('.rs')}/")
                        continue
                    owner = module_directory(relative)
                    files.add(f"{owner}/{name}.rs")
                    directories.add(f"{owner}/{name}/")
        _TEST_MODULES = (frozenset(files), tuple(sorted(directories)))
    return _TEST_MODULES


def test_path(relative: str) -> bool:
    """Whether a file is test code by where it sits, before reading a line of it.

    A `tests` directory is the obvious case; a `tests.rs` beside the module it
    tests is the same claim spelled differently, and treating it as shipped code
    is how a probe usage passes for a consumer.  A file some parent declares as
    `#[cfg(test)] mod ...;` is the same claim again, written where the file
    itself cannot show it.
    """
    parts = relative.split("/")
    if (
        "tests" in parts
        or "benches" in parts
        or parts[-1].removesuffix(".rs") in {"test", "tests", "bench", "benches"}
    ):
        return True
    files, directories = test_module_paths()
    return relative in files or any(
        relative.startswith(directory) for directory in directories
    )


def feature_gated_test_home(relative: str) -> bool:
    """Whether a file is one of the `testing` / `test_support` homes.

    These are the modules `FEATURE_GATED_TEST_HOMES` names as where a test-only
    item is *relocated to*: `lash_core::testing`, `lash_core::test_support`, the
    conformance harness under them, and the same modules in `lash`, `lashlang`
    and the providers.  Their files are shipped in the sense the compiler cares
    about -- a downstream build can turn the feature on -- and reading them as
    `crate-src` would let a row prove an internal seam by citing the test
    harness, which is the amnesty this ledger spent FIG-1223 closing.  A path
    rule, deliberately: the registry's own doctrine names these modules as the
    home a `Relocate:` note points at, so a file that lives in one answers to
    the test tiers no matter which feature compiles it.
    """
    homes = tuple(home.removeprefix("::") for home in FEATURE_GATED_TEST_HOMES)
    for segment in relative.removesuffix(".rs").split("/"):
        for home in homes:
            if (
                segment == home
                or segment.startswith(f"{home}_")
                or segment.endswith(f"_{home}")
            ):
                return True
    return False


def anchor_tier(reference: str) -> str | None:
    """Which evidence tier an anchor's path shape places it in.

    The tier is a property of where the code lives, never of what the row says
    about it: an example's host code, an example's tests, another crate's `src/`,
    or a crate's `tests/`.  See the module docstring for what each one proves.
    """
    location = anchor_location(reference)
    if location is None:
        return None
    relative, line_number = location
    root = relative.split("/", 1)[0]
    if root not in ANCHOR_ROOTS:
        return None
    regions = cfg_test_regions(relative) or []
    # A `#[cfg(test)]` block is test code wherever it lives. The session picker
    # looked alive because a probe usage sat in one of these inside the crate
    # that defined it, so shipped code and test code cannot share a tier just
    # because they share a file.
    in_test = test_path(relative) or feature_gated_test_home(relative) or any(
        start <= line_number <= end for start, end in regions
    )
    if root == "crates":
        return "workspace-tests" if in_test else "crate-src"
    return "example-test" if in_test else "example-host"


def anchor_crate(reference: str) -> str | None:
    """The workspace crate directory an anchor points into, if it is in one."""
    location = anchor_location(reference)
    if location is None:
        return None
    parts = location[0].split("/")
    if parts[0] != "crates" or len(parts) < 2:
        return None
    return f"crates/{parts[1]}"


def source_file_lines(relative: str) -> list[str] | None:
    """A repository file's lines, read once per run.

    Every evidence lint reads the same handful of source files over and over --
    the anchor's line, its `cfg(test)` regions, the function around it -- so the
    cache is what keeps a per-row rule from being a per-row file read.
    """
    if relative not in _SOURCE_LINES:
        try:
            _SOURCE_LINES[relative] = (
                (REPO / relative).read_text(encoding="utf-8").splitlines()
            )
        except OSError:
            _SOURCE_LINES[relative] = None
    return _SOURCE_LINES[relative]


def anchored_source(reference: str) -> str | None:
    """The source line a reference points at, or None when it does not resolve."""
    location = anchor_location(reference)
    if location is None:
        return None
    relative, line_number = location
    path = REPO / relative
    if not any(path.is_relative_to(REPO / root) for root in ANCHOR_ROOTS):
        return None
    lines = source_file_lines(relative)
    if lines is None or line_number > len(lines):
        return None
    return lines[line_number - 1]


def anchored_exercise_source(reference: str) -> str | None:
    """The source line, widened to its `use` prefix for multiline imports."""
    source = anchored_source(reference)
    if source is None:
        return None
    try:
        location, _ = reference.split("#", 1)
        relative, line_text = location.rsplit(":", 1)
        line_number = int(line_text)
    except (ValueError, TypeError):
        return source
    lines = source_file_lines(relative) or []
    target = line_number - 1
    for index in range(target, max(-1, target - 20), -1):
        candidate = lines[index].strip()
        if IMPORT_ONLY_EXERCISE.match(candidate):
            if index == target or ";" not in candidate:
                return "\n".join(lines[index : target + 1])
            break
        if index < target and ";" in candidate:
            break
    return source


def reference_exists(reference: str) -> bool:
    source = anchored_source(reference)
    return source is not None and reference.split("#", 1)[-1] in source


def resolved_internal_reference(reference: str) -> str | None:
    """Resolve an internal anchor after non-semantic line movement.

    Internal consumers live in workspace source where documentation-only edits
    routinely move the recorded line.  The exact source snippet remains the
    durable identity; relocation stays within the recorded file, and the caller
    still applies import, declaration, and member-owner checks to the resolved
    line.
    """
    if reference_exists(reference):
        return reference
    relocated = relocated_reference(reference)
    if relocated is not None and reference_exists(relocated):
        return relocated
    return None


#: Kinds whose name means nothing without the item that owns it: `id` is a
#: hundred fields, `AcceptedInjectedTurnInput::id` is one.
MEMBER_KINDS = {"field", "variant", "function", "assoc_const", "assoc_type"}
#: A function definition's opening line, in any of the orders Rust allows.
FUNCTION_HEADER = re.compile(
    r"^(?:pub(?:\s*\([^)]*\))?\s+)?(?:default\s+)?(?:const\s+)?(?:async\s+)?"
    r"(?:unsafe\s+)?(?:extern\s+\"[^\"]*\"\s+)?fn\s"
)


def member_owners(symbol: str, aliases: Any, kind: str) -> set[str]:
    """The type names that could legitimately introduce this member on a line.

    Every path the item is visible under contributes one, because a consumer
    writes whichever path it imported.
    """
    if kind not in MEMBER_KINDS:
        return set()
    owners = set()
    for path in [symbol, *(aliases or [])]:
        segments = str(path).split("::")
        if len(segments) > 1:
            owners.add(segments[-2])
    return owners


def member_leaf_owners(entries: list[dict[str, Any]]) -> dict[str, set[str]]:
    """Which types each member name belongs to, across every path in the ledger.

    A leaf shared by two items is a leaf no anchor can identify on its own.
    """
    owners: dict[str, set[str]] = {}
    for entry in entries:
        if entry.get("kind") not in MEMBER_KINDS:
            continue
        for path in [entry.get("symbol", ""), *(entry.get("aliases") or [])]:
            segments = str(path).split("::")
            if len(segments) > 1:
                owners.setdefault(segments[-1], set()).add(segments[-2])
    return owners


def names_word(name: str, text: str) -> bool:
    """Whether `text` names `name` as a whole identifier."""
    return (
        re.search(rf"(?<![A-Za-z0-9_]){re.escape(name)}(?![A-Za-z0-9_])", text)
        is not None
    )


def block_header(lines: list[str], index: int) -> tuple[str, int]:
    """The kind of block opening at `index`, and the line its header starts on.

    A signature that spans lines opens its block on `) -> Result<_, _> {`, which
    names nothing.  Reading only that line makes the enclosing function
    unrecognizable, and a function nobody can find becomes a scope the size of the
    file -- which is how two different `context:` parameters ended up in one
    scope and a receiver resolved to both (FIG-1223).

    The walk back is bounded by the first line that cannot be part of a signature
    -- a blank line, a comment, an attribute, or a line ending a statement or a
    block -- so the only thing a line budget adds is a ceiling on how long a
    parameter list may be.  Fourteen lines was under the longest constructors
    here -- `ProcessEngineRunContext::new` takes seventeen parameters -- so their
    bodies had no enclosing function, which was invisible while that meant the
    whole file and would have made every citation into one unconfirmable once an
    unenclosed line means nothing at all (FIG-1526).
    """
    for position in range(index, max(-1, index - 60), -1):
        stripped = lines[position].strip()
        if FUNCTION_HEADER.match(stripped):
            return "fn", position
        if stripped.startswith("impl"):
            return "impl", position
        if position < index and (
            not stripped
            or stripped.endswith((";", "{", "}"))
            or stripped.startswith(("//", "#["))
        ):
            break
    return "", index


def scope_blocks(lines: list[str]) -> list[tuple[int, int, str]]:
    """`(start, end, kind)` for every `fn` and `impl` block in a source file.

    Computed once per file: a per-anchor brace walk is a per-anchor pass over the
    whole file, and the member rule asks this question of every candidate line.
    """
    depth = 0
    open_blocks: list[list[Any]] = []
    blocks: list[tuple[int, int, str]] = []
    for index, line in enumerate(lines):
        stripped = line.strip()
        opened = line.count("{")
        if opened:
            kind, header = block_header(lines, index)
            open_blocks.append([depth, header, kind])
        depth += opened - line.count("}")
        while open_blocks and depth <= open_blocks[-1][0]:
            start, position, kind = open_blocks.pop()
            if kind:
                blocks.append((position, index, kind))
    for start, position, kind in open_blocks:
        if kind:
            blocks.append((position, len(lines) - 1, kind))
    return sorted(blocks)


def file_scope_blocks(relative: str) -> list[tuple[int, int, str]]:
    """`scope_blocks` for a repository file, read and parsed once per run."""
    if relative not in _SCOPE_BLOCKS:
        lines = source_file_lines(relative)
        _SCOPE_BLOCKS[relative] = [] if lines is None else scope_blocks(lines)
    return _SCOPE_BLOCKS[relative]


#: A named item that opens a block: `mod`, `fn`, `struct`, `enum`, `trait`, `union`.
#: Qualifiers a declaration may carry before its keyword are consumed first, so
#: `pub(crate) async unsafe fn drain` names `drain` and not `async`.
SYMBOL_BLOCK = re.compile(
    r"^(?:pub(?:\s*\([^)]*\))?\s+)?(?:default\s+)?"
    r"(?:(?:const|async|unsafe|extern\s+\"[^\"]*\")\s+)*"
    r"(?P<keyword>mod|fn|struct|enum|trait|union)\s+(?P<name>[A-Za-z_]\w*)"
)


def symbol_header(lines: list[str], index: int) -> tuple[str | None, int]:
    """The name of the item whose block opens at `index`, and its header line.

    The walk back matches `block_header`'s, and for the same reason: a signature
    that spans lines opens its block on a line naming nothing, and an item nobody
    can find is not a citable anchor.  An `impl` block is named by its *type* --
    `impl Store for PgSessionStore` is where `PgSessionStore`'s methods live, so
    a citation writes `PgSessionStore::connect` and never spells the trait.
    """
    for position in range(index, max(-1, index - 60), -1):
        stripped = lines[position].strip()
        declaration = SYMBOL_BLOCK.match(stripped)
        if declaration:
            return declaration.group("name"), position
        implementation = IMPL_HEADER.match(stripped)
        if implementation:
            return implementation.group("type").split("::")[-1], position
        if position < index and (
            not stripped
            or stripped.endswith((";", "{", "}"))
            or stripped.startswith(("//", "#["))
        ):
            break
    return None, index


def symbol_blocks(lines: list[str]) -> list[tuple[int, int, str]]:
    """`(start, end, name)` for every named block a file declares.

    The anchor identity a prose citation resolves against (FIG-1550).  Line
    numbers move on every edit above them; a symbol moves only when someone
    moves or deletes it, which is exactly when a human should re-read the claim.
    """
    depth = 0
    open_blocks: list[tuple[int, int, str | None]] = []
    blocks: list[tuple[int, int, str]] = []
    for index, line in enumerate(lines):
        opened = line.count("{")
        if opened:
            name, header = symbol_header(lines, index)
            # A brace inside a body can walk back to the same declaration -- a
            # struct literal opening two lines under a signature reads as the
            # signature's own block.  The outermost brace that header opens is
            # the real one, so a repeat is left unnamed rather than nesting a
            # symbol inside itself.
            if any(open_header == header for _, open_header, _ in open_blocks):
                name = None
            open_blocks.append((depth, header, name))
        depth += opened - line.count("}")
        while open_blocks and depth <= open_blocks[-1][0]:
            _, position, name = open_blocks.pop()
            if name:
                blocks.append((position, index, name))
    for _, position, name in open_blocks:
        if name:
            blocks.append((position, len(lines) - 1, name))
    return sorted(blocks)


def file_symbol_blocks(relative: str) -> list[tuple[int, int, str]]:
    """`symbol_blocks` for a repository file, read and parsed once per run."""
    if relative not in _SYMBOL_BLOCKS:
        lines = source_file_lines(relative)
        _SYMBOL_BLOCKS[relative] = [] if lines is None else symbol_blocks(lines)
    return _SYMBOL_BLOCKS[relative]


def symbol_spans(relative: str, path: str) -> list[tuple[tuple[int, int], ...]]:
    """Every chain of regions in a file that a `A::B::c` symbol path names.

    Each segment is resolved inside the region the previous one won, at any
    nesting depth: `PgSessionStore::connect` finds the method wherever the impl
    block sits, and `tests::round_trips` finds a test inside its `mod tests`.
    More than one chain is not an error -- two impl blocks may carry the same
    method name, and a citation that lands in either is a citation to a real
    symbol.  Zero chains is the break the ruling wants loud: the symbol moved
    files or died.

    A chain rather than its last region, because the ancestors are what name the
    owner: `fn begin(&self, ...)` proves nothing on its own and
    `impl RuntimeTurnPhaseProbe for WorkbenchProbe` above it proves everything,
    which is the same window `anchor_scope_text` reads a line anchor in.
    """
    lines = source_file_lines(relative)
    if lines is None:
        return []
    blocks = file_symbol_blocks(relative)
    chains: list[tuple[tuple[int, int], ...]] = [()]
    for segment in path.split("::"):
        nested: list[tuple[tuple[int, int], ...]] = []
        for chain in chains:
            low, high = chain[-1] if chain else (-1, len(lines))
            for start, end, name in blocks:
                if name != segment or start < low or end > high:
                    continue
                if (start, end) != (low, high) and (*chain, (start, end)) not in nested:
                    nested.append((*chain, (start, end)))
        if not nested:
            return []
        chains = nested
    return chains


def symbol_span_text(relative: str, chain: tuple[tuple[int, int], ...]) -> str:
    """The source a symbol spans: its own block, under each ancestor's header."""
    lines = source_file_lines(relative) or []
    start, end = chain[-1]
    return "\n".join([*(lines[span[0]] for span in chain[:-1]), *lines[start : end + 1]])


def enclosing_symbol_path(relative: str, line_number: int) -> str | None:
    """The `A::B::c` path of the innermost named blocks around a line.

    The inverse of `symbol_spans`, and the only thing the FIG-1550 migration
    needed: it reads a line pin recorded against a file and answers which symbol
    the pin was pointing inside.
    """
    target = line_number - 1
    containing = [
        (start, end, name)
        for start, end, name in file_symbol_blocks(relative)
        if start <= target <= end
    ]
    if not containing:
        return None
    # Outermost first, and a block sharing its start with the one it contains is
    # still the outer one: sorting by end alone would invert such a pair and
    # produce a path no resolution can walk.
    containing.sort(key=lambda block: (block[0], -block[1]))
    return "::".join(name for _, _, name in containing)


def anchor_scope(relative: str, line_number: int) -> str:
    """The innermost function containing a line, cached by the region it spans."""
    key = (relative, *anchor_scope_region(relative, line_number))
    if key not in _ANCHOR_SCOPES:
        _ANCHOR_SCOPES[key] = anchor_scope_text(relative, line_number)
    return _ANCHOR_SCOPES[key]


def anchor_scope_region(relative: str, line_number: int) -> tuple[int, int]:
    """The `(start, end)` of the innermost function block around a line."""
    target = line_number - 1
    body = (-1, -1)
    for start, end, kind in file_scope_blocks(relative):
        if start > target or target > end or kind == "impl":
            continue
        if start > body[0]:
            body = (start, end)
    return body


def anchor_scope_text(relative: str, line_number: int) -> str:
    """The innermost function containing a line, plus its `impl` headers.

    The window a member anchor is read in.  A function is the smallest region
    where a receiver's type is established, and the `impl` header is where a
    trait method's owner is named -- `fn begin(&self, ...)` proves nothing on its
    own, `impl RuntimeTurnPhaseProbe for ...` above it proves everything.

    A line outside every function has no such window, and reading the whole file
    as its scope is how a citation to a blank line, a `}`, or an attribute passed:
    the symbol appears *somewhere* in the file, so the check saw its own name and
    agreed.  FIG-1526 made that case an empty scope, which no name matches.
    """
    lines = source_file_lines(relative) or []
    target = line_number - 1
    body: tuple[int, int] | None = None
    headers: list[int] = []
    for start, end, kind in file_scope_blocks(relative):
        if start > target or target > end:
            continue
        if kind == "impl":
            headers.append(start)
        elif body is None or start > body[0]:
            body = (start, end)
    if body is None:
        return ""
    region = lines[body[0] : body[1] + 1]
    return "\n".join([*(lines[index] for index in headers), *region])


def member_containers(symbol: str, aliases: Any, kind: str) -> set[str]:
    """The types a nested member sits inside, beyond its immediate owner.

    A variant's field is owned by the variant, and variant names are as generic
    as member names -- `Message`, `Text`, `Error`. Naming the variant alone left
    `BorrowedChronologicalPayload::Message::0` "proved" by
    `use tokio_tungstenite::...::Message as WsMessage;`. Only CamelCase segments
    qualify, so a module path never becomes a requirement.
    """
    if kind not in MEMBER_KINDS:
        return set()
    containers = set()
    for path in [symbol, *(aliases or [])]:
        segments = str(path).split("::")
        if len(segments) > 2 and segments[-3][:1].isupper():
            containers.add(segments[-3])
    return containers


def imported_type_names(relative: str) -> set[str]:
    """Type names a file imports, from its `use` tree, aliases included.

    The rival set cannot come from this inventory alone: the motivating
    coincidence was `serde_json`'s `Value::as_str` and an alias of
    `tungstenite`'s `Message`, neither of which is Lash API at all.  Any imported
    type in the file is a candidate subject for a member access the line does not
    qualify (FIG-1223).
    """
    if relative not in _IMPORTED_TYPES:
        names: set[str] = set()
        for line in source_file_lines(relative) or []:
            stripped = line.strip()
            if not stripped.startswith(("use ", "pub use ", "pub(crate) use ")):
                continue
            names.update(
                name
                for name in re.findall(r"[A-Za-z_][A-Za-z0-9_]*", stripped)
                if name[:1].isupper()
            )
        _IMPORTED_TYPES[relative] = names
    return _IMPORTED_TYPES[relative]


def qualifies_member(owner: str, leaf: str, snippet: str) -> bool:
    """Whether a line ties a member to its owner outright, not by proximity.

    `Owner::leaf`, `Owner { leaf, .. }` and a tuple variant's `Owner(..)` name
    both halves of the contract on one line.  A line that merely mentions the
    owner elsewhere does not: `RemoteInputItem::Text { text: text.into() }`
    mentions plenty and settles nothing.
    """
    owner_pattern = re.escape(owner)
    leaf_pattern = re.escape(leaf)
    if re.search(rf"(?<![A-Za-z0-9_]){owner_pattern}\s*::\s*{leaf_pattern}(?![A-Za-z0-9_])", snippet):
        return True
    brace = re.search(rf"(?<![A-Za-z0-9_]){owner_pattern}\s*\{{", snippet)
    if brace and names_word(leaf, snippet[brace.end() :]):
        return True
    if leaf.isdigit() and re.search(
        rf"(?<![A-Za-z0-9_]){owner_pattern}\s*\(", snippet
    ):
        return True
    return False


def anchor_receiver(leaf: str, snippet: str) -> str | None:
    """The identifier a member is reached through on this line, if any."""
    match = re.search(
        rf"(?<![A-Za-z0-9_.])([A-Za-z_][A-Za-z0-9_]*)\s*\.\s*{re.escape(leaf)}"
        r"(?![A-Za-z0-9_])",
        snippet,
    )
    return None if match is None else match.group(1)


def binds_receiver(scope: str, receiver: str, owners: set[str]) -> bool:
    """Whether the surrounding function ties this receiver to the owning type.

    `value.as_str()` and `entry.payload` are the same shape and different
    evidence: one file binds `value: &Value`, the other binds `entry` from a
    `BorrowedChronologicalEntry`.  Reading the binding is what separates them, and
    it is the difference between a coincidence and a consumer (FIG-1223).
    """
    name = re.escape(receiver)
    for owner in owners:
        pattern = re.escape(owner)
        shapes = (
            rf"{name}\s*:\s*[^,;)=]*(?<![A-Za-z0-9_]){pattern}(?![A-Za-z0-9_])",
            rf"let\s+(?:mut\s+)?{name}\s*(?::[^=;]*|=[^;]*)"
            rf"(?<![A-Za-z0-9_]){pattern}(?![A-Za-z0-9_])",
            rf"{name}\s*=\s*[^;]*(?<![A-Za-z0-9_]){pattern}(?![A-Za-z0-9_])",
            rf"for\s+{name}\s+in\s+[^{{]*(?<![A-Za-z0-9_]){pattern}(?![A-Za-z0-9_])",
        )
        if any(re.search(shape, scope) for shape in shapes):
            return True
    return False


#: `impl Trait for Type`, `impl Type`, and the generics either may carry.
IMPL_HEADER = re.compile(
    r"^impl(?:\s*<.*?>)?\s+(?:(?P<trait>[A-Za-z_][\w:<>, '&]*?)\s+for\s+)?"
    r"(?P<type>[A-Za-z_][\w:]*)"
)
#: `type Alias = Real<..>;`, which is often where the real type is named.
ALIAS_HEADER = re.compile(r"^(?:pub(?:\s*\([^)]*\))?\s+)?type\s+([A-Z]\w*)\s*(?:<[^>]*>)?\s*=")
#: `struct X`, `enum X`, `trait X` -- the container a following field belongs to.
CONTAINER_HEADER = re.compile(
    r"^(?:pub(?:\s*\([^)]*\))?\s+)?(?P<keyword>struct|enum|trait|union)\s+(?P<name>[A-Za-z_]\w*)"
)
#: `Part(LlmOutputPart)` -- an enum variant carrying a payload type.
VARIANT_PAYLOAD = re.compile(r"^(?P<name>[A-Z]\w*)\s*\((?P<payload>[^()]*(?:\([^()]*\)[^()]*)*)\)")
#: `name: Type` in a field or parameter position.
TYPED_NAME = re.compile(r"^(?:pub(?:\s*\([^)]*\))?\s+)?([a-z_]\w*)\s*:\s*(.+?),?$")
#: `fn name(...) -> Return`
TYPED_FUNCTION = re.compile(r"\bfn\s+([a-z_]\w*)\s*(?:<[^>]*>)?\s*\(.*?\)\s*->\s*(.+?)\s*\{?$")
#: A CamelCase type name inside a type expression, wrappers and all.
TYPE_NAME = re.compile(r"(?<![A-Za-z0-9_])([A-Z]\w*)")
_TYPE_FACTS: dict[str, Any] = {}
_LITERAL_STACKS: dict[str, list[list[str]]] = {}
_ANCHOR_SCOPES: dict[tuple[str, int, int], str] = {}
_RESOLVED_RECEIVERS: dict[tuple[Any, ...], set[str]] = {}


def index_targets() -> list[str]:
    """Every source file the type index reads: on disk, or supplied by a test."""
    found = {
        path.relative_to(REPO).as_posix()
        for root in ANCHOR_ROOTS
        for path in (REPO / root).rglob("*.rs")
    }
    found.update(
        relative
        for relative, lines in _SOURCE_LINES.items()
        if lines and relative.split("/", 1)[0] in ANCHOR_ROOTS
    )
    return sorted(found)


def type_facts() -> dict[str, Any]:
    """A textual model of the workspace's types, built once per run.

    The consumer search has to answer "is this call about *that* type", and a
    name match cannot: `self.input.turn_context.clear_prompt_slot(slot)` names no
    type at all, and its member belongs to a trait two hops away.  So the index
    records what the source says out loud -- a struct's field types, a method's
    return type, and which traits each type implements -- which is enough to walk
    a receiver expression back to a type and then to the trait that owns the
    member.  Textual, not semantic: it resolves what a reader resolves.
    """
    if _TYPE_FACTS:
        return _TYPE_FACTS
    fields: dict[str, dict[str, str]] = {}
    methods: dict[str, dict[str, str]] = {}
    traits: dict[str, set[str]] = {}
    impls: dict[str, list[tuple[int, int, str, str]]] = {}
    aliases: dict[str, str] = {}
    variants: dict[str, dict[str, str]] = {}
    payloads: dict[str, set[str]] = {}
    declarations: dict[str, list[tuple[int, int, str, str]]] = {}
    enums: set[str] = set()
    for relative in index_targets():
        lines = source_file_lines(relative)
        if lines is None:
            continue
        blocks: list[tuple[int, int, str]] = []
        depth = 0
        open_blocks: list[list[Any]] = []
        for index, line in enumerate(lines):
            stripped = line.strip()
            alias = ALIAS_HEADER.match(stripped)
            if alias:
                # `type X = Driver<..>;` may wrap to the next lines, and the
                # alias is often the only place the real type is named.
                text = stripped
                following = index
                while ";" not in text and following + 1 < len(lines):
                    following += 1
                    text += " " + lines[following].strip()
                aliases[alias.group(1)] = text.split("=", 1)[1]
            header: tuple[str, str] | None = None
            impl_match = IMPL_HEADER.match(stripped)
            container = CONTAINER_HEADER.match(stripped)
            if impl_match:
                owner = impl_match.group("type").split("::")[-1]
                # Only the trait's own name: `impl From<TokenUsage> for RemoteUsage`
                # implements `From`, and reading its argument as a trait made every
                # `self.field` in RemoteUsage evidence for TokenUsage's field.
                head = re.match(r"[A-Za-z_][\w:]*", (impl_match.group("trait") or "").strip())
                trait_names = [head.group(0).split("::")[-1]] if head else []
                if trait_names:
                    traits.setdefault(owner, set()).update(trait_names)
                header = ("impl", owner)
                impls.setdefault(relative, []).append(
                    (index, index, owner, trait_names[0] if trait_names else "")
                )
            elif container:
                name = container.group("name")
                header = ("container", name)
                if container.group("keyword") == "enum":
                    enums.add(name)
                declarations.setdefault(relative, []).append(
                    (index, index, name, container.group("keyword"))
                )
            opened = line.count("{")
            if opened:
                open_blocks.append([depth, index, header])
            depth += opened - line.count("}")
            while open_blocks and depth <= open_blocks[-1][0]:
                start, position, kind = open_blocks.pop()
                if kind is not None:
                    blocks.append((position, index, kind[1]))
                    source = impls if kind[0] == "impl" else declarations
                    for slot, entry in enumerate(source.get(relative, [])):
                        if entry[0] == position:
                            source[relative][slot] = (position, index, entry[2], entry[3])
            owner = next(
                (
                    name
                    for begin, _, name in reversed(blocks + [
                        (start, index, kind[1])
                        for start, _, kind in [
                            (block[1], index, block[2])
                            for block in open_blocks
                            if block[2] is not None
                        ]
                    ])
                    if begin <= index
                ),
                None,
            )
            if owner is None:
                continue
            if owner in enums:
                # `Part(LlmOutputPart)` is where a variant's payload type is
                # declared, and `if let ..::Part(part)` is how a consumer binds
                # it -- without the pair, a destructured binding has no type.
                variant = VARIANT_PAYLOAD.match(stripped)
                if variant:
                    payload = variant.group("payload")
                    variants.setdefault(owner, {})[variant.group("name")] = payload
                    payloads.setdefault(variant.group("name"), set()).add(payload)
            function = TYPED_FUNCTION.search(stripped)
            if function:
                methods.setdefault(owner, {})[function.group(1)] = function.group(2)
                continue
            typed = TYPED_NAME.match(stripped)
            if typed:
                fields.setdefault(owner, {}).setdefault(typed.group(1), typed.group(2))
    _TYPE_FACTS.update(
        {
            "fields": fields,
            "methods": methods,
            "traits": traits,
            "impls": impls,
            "aliases": aliases,
            "variants": variants,
            "payloads": payloads,
            "declarations": declarations,
        }
    )
    return _TYPE_FACTS


def type_names(expression: str | None, expand: bool = True) -> set[str]:
    """The type names a type expression mentions, wrappers and aliases resolved."""
    if not expression:
        return set()
    names = {
        name
        for name in TYPE_NAME.findall(expression)
        if name not in {"Self", "Option", "Result", "Vec", "Box", "Arc", "Rc", "String"}
    }
    if not expand:
        return names
    aliases = type_facts()["aliases"]
    resolved = set(names)
    for name in names:
        target = aliases.get(name)
        if target:
            resolved |= type_names(target, expand=False)
    return resolved


def impl_owner(relative: str, line_number: int) -> set[str]:
    """The types whose `impl` blocks contain a line, innermost first."""
    target = line_number - 1
    return {
        owner
        for start, end, owner, _ in type_facts()["impls"].get(relative, [])
        if start <= target <= end
    }


def pattern_binding_types(scope: str, name: str) -> set[str]:
    """Types a destructuring pattern gives a local name.

    `if let LlmStreamEvent::Part(part) = &mut event` is the only line that types
    `part`, and it types it through the variant's declared payload.  A binding the
    resolver cannot read is a receiver it must reject, so a rule that fails closed
    has to know this shape or it calls a live consumer dead (FIG-1223).
    """
    binding = rf"(?:ref\s+)?(?:mut\s+)?&?(?:mut\s+)?{re.escape(name)}\s*[,)]"
    found: set[str] = set()
    facts = type_facts()
    for pattern in (
        rf"(?:if|while)\s+let\s+([\w:]+)\s*\(\s*{binding}",
        # A match arm, anchored at the start of the arm: `foo(entry)` in an
        # argument list is a call, not a pattern, and reading it as one gave a
        # receiver every payload type in the workspace.
        rf"(?m)^\s*(?:\|\s*)?([\w:]+)\s*\(\s*{binding}\s*(?:if\b[^=]*)?=>",
    ):
        for match in re.finditer(pattern, scope):
            segments = match.group(1).split("::")
            variant = segments[-1]
            if len(segments) > 1:
                owner = segments[-2]
                declared = facts["variants"].get(owner, {}).get(variant)
                if declared:
                    found |= type_names(declared)
                    continue
            if variant in {"Some", "Ok", "Err", "None"}:
                # `Some(entry)` says nothing about the type; the map it came from
                # does, and that is beyond a textual read.
                continue
            declared = facts["payloads"].get(variant, set())
            if len(declared) == 1:
                # An unqualified variant is only a type when the workspace
                # declares exactly one variant by that name.
                found |= type_names(next(iter(declared)))
    return found


def local_binding_types(scope: str, name: str) -> set[str]:
    """Types the surrounding function gives a local name.

    Read the way a reader reads it: the signature types a parameter, a `let`
    types its binding by annotation or by the constructor on its right-hand side.
    Anything looser answers with the whole function -- a hundred-line body writes
    `entry:` in a dozen literals, and taking all of them gave one receiver forty
    types and made the tie meaningless (FIG-1223).
    """
    found: set[str] = pattern_binding_types(scope, name)
    position = scope.find("fn ")
    signature = scope if position < 0 else scope[position : scope.find("{", position)]
    binding = re.escape(name)
    for pattern, text in (
        (rf"(?<![A-Za-z0-9_]){binding}\s*:\s*([^,;)=]+)", signature),
        (rf"let\s+(?:mut\s+)?{binding}\s*:\s*([^=;]+)", scope),
        (
            rf"let\s+(?:mut\s+)?{binding}\s*=\s*(?:&\s*)?(?:mut\s+)?"
            rf"([A-Z]\w*)\s*(?:::|\{{|\()",
            scope,
        ),
        (rf"for\s+{binding}\s+in\s+([^{{]+)", scope),
    ):
        for match in re.finditer(pattern, text):
            for group in match.groups():
                found |= type_names(group)
    return found


def resolve_receiver_types(relative: str, line_number: int, chain: str) -> set[str]:
    """`resolve_receiver_chain`, cached per function region and expression."""
    key = (relative, *anchor_scope_region(relative, line_number), chain)
    if key not in _RESOLVED_RECEIVERS:
        _RESOLVED_RECEIVERS[key] = resolve_receiver_chain(relative, line_number, chain)
    return _RESOLVED_RECEIVERS[key]


def resolve_receiver_chain(relative: str, line_number: int, chain: str) -> set[str]:
    """The types a receiver expression can have, walked one hop at a time.

    `self.input.turn_context` is three hops: the `impl` block gives `self`, a
    struct field gives `input`, another gives `turn_context`.  Each hop is a
    lookup in `type_facts`, and an unresolvable hop returns nothing rather than
    guessing -- an empty answer fails the anchor rather than passing it.
    """
    facts = type_facts()
    scope = anchor_scope(relative, line_number)
    parts = [part.strip() for part in chain.split(".") if part.strip()]
    # `self.tool_state().await?.generation()` navigates twice and *unwraps* once:
    # the future and the `Result` are not hops, and treating them as unknown hops
    # made every fallible accessor unresolvable -- and its path deletable.
    parts = [part for part in parts if part not in PEELED_HOPS]
    parts = [part[:-1] if part.endswith("?") else part for part in parts]
    if not parts:
        return set()
    head, *rest = parts
    if head == "self":
        current = impl_owner(relative, line_number)
    else:
        current = local_binding_types(scope, head.removesuffix("()"))
        if head.endswith("()"):
            current |= {
                name
                for owner in impl_owner(relative, line_number)
                for name in type_names(facts["methods"].get(owner, {}).get(head[:-2]))
            }
    for part in rest:
        name = part.removesuffix("()")
        following: set[str] = set()
        for owner in current:
            if part.endswith("()"):
                following |= type_names(facts["methods"].get(owner, {}).get(name))
            else:
                following |= type_names(facts["fields"].get(owner, {}).get(name))
        current = following
        if not current:
            return set()
    return current


def ties_receiver(
    owners: set[str], relative: str, line_number: int, chain: str
) -> bool:
    """Whether a receiver expression resolves to a type the member belongs to.

    Directly, or through the trait: `retire_effect_journal` is owned by a trait,
    the receiver is a store, and `impl EffectReplayDriver for ...` is the line
    that ties them together.
    """
    resolved = resolve_receiver_types(relative, line_number, chain)
    if resolved & owners:
        return True
    implemented = {
        trait
        for candidate in resolved
        for trait in type_facts()["traits"].get(candidate, set())
    }
    return bool(implemented & owners)


#: A member reached through an expression: `x.leaf`, `self.a.b().leaf`.
RECEIVER_CHAIN = re.compile(
    r"((?:[A-Za-z_]\w*\s*(?:\(\s*\))?\s*\??\s*\.\s*)+)$"
)
#: Hops that unwrap rather than navigate: `?`, `.await`, `.await?`.
PEELED_HOPS = {"await", "await?", "?"}


#: A line whose expression continues on the next one.
CONTINUES = ("(", ".", ",", "=", "&", "|", "+", "-", "*", "/", "?", "<", ">", ":", "!")


def expression_prefix(relative: str, line_number: int, leaf: str, snippet: str) -> str:
    """The text leading up to a member on its line, continuation lines included.

    A fluent chain is written down the page -- `effect_host` on one line,
    `.scoped_static(..)` on the next -- so reading the anchor's own line finds no
    receiver at all.  The prefix is assembled backwards while the lines say they
    continue, and only the prefix: the leaf is located on the anchor's own line so
    an earlier line writing the same name cannot move it (FIG-1223).
    """
    lines = source_file_lines(relative) or []
    index = line_number - 1
    if index < 0 or index >= len(lines):
        return snippet.split(leaf)[0]
    line = lines[index]
    match = re.search(rf"(?<![A-Za-z0-9_]){re.escape(leaf)}(?![A-Za-z0-9_])", line)
    head = line[: match.start()] if match else line
    first = index
    while first > 0:
        previous = lines[first - 1].strip()
        current = lines[first].strip()
        if not previous or previous.endswith((";", "{", "}")):
            break
        if current.startswith((".", "?")) or previous.endswith(CONTINUES):
            first -= 1
            continue
        break
    return " ".join([*(lines[position].strip() for position in range(first, index)), head])


#: How far below its opening brace a literal's field can be and still be read as
#: part of it.  A brace walk over text drifts -- macros and `cfg` blocks leave it
#: unbalanced -- and a literal whose opening line is off the screen is not
#: something a reader would attribute a field to.
LITERAL_REACH = 60
#: A string or char literal, escapes included.
STRING_LITERAL = re.compile(r"\"(?:\\.|[^\"\\])*\"|'(?:\\.|[^'\\])'")


def file_literal_stacks(relative: str) -> list[list[str]]:
    """For each line of a file, the named `{` blocks it sits inside.

    Computed in one pass per file: the member rule asks this of every candidate
    line, and a per-line brace walk is a per-line pass over the whole file.
    """
    if relative in _LITERAL_STACKS:
        return _LITERAL_STACKS[relative]
    lines = source_file_lines(relative)
    if lines is None:
        _LITERAL_STACKS[relative] = []
        return []
    starts = {start for start, _, _ in file_scope_blocks(relative)}
    stack: list[tuple[str | None, int]] = []
    per_line: list[list[str]] = []
    for index, raw in enumerate(lines):
        # Braces inside string literals, char literals and comments are not
        # blocks: a `format!("{spec}")` or a lone `"{"` unbalances the walk, and
        # then a line inherits a literal that closed hundreds of lines earlier.
        line = STRING_LITERAL.sub('""', raw)
        line = line.split("//", 1)[0] if "//" in line else line
        per_line.append(
            [
                name
                for name, opened_at in stack
                if name and index - opened_at <= LITERAL_REACH
            ]
        )
        for position, character in enumerate(line):
            if character == "{":
                name = re.search(r"([A-Za-z_][\w:]*)\s*$", line[:position].rstrip())
                candidate = name.group(1).split("::")[-1] if name else None
                if index in starts:
                    # `fn make() -> ModelToolReturn {` opens a function, not a
                    # literal; its return type is not a literal a field sits in.
                    candidate = None
                stack.append(
                    (candidate if candidate and candidate[:1].isupper() else None, index)
                )
            elif character == "}" and stack:
                stack.pop()
    _LITERAL_STACKS[relative] = per_line
    return per_line


def literal_stack(relative: str, line_number: int) -> list[str]:
    """The named `{` blocks a line sits inside, outermost first."""
    stacks = file_literal_stacks(relative)
    index = line_number - 1
    return stacks[index] if 0 <= index < len(stacks) else []


def declaration_owner(relative: str, line_number: int) -> str | None:
    """The type whose `struct`/`enum`/`trait` body a line sits in, if any.

    A field *declaration* is not a consumer of the same-named field of some other
    type: `attempt: usize,` inside another crate's own `RetryStatus` variant is
    that crate declaring its own wire enum (FIG-1223).
    """
    target = line_number - 1
    for start, end, name, _ in type_facts()["declarations"].get(relative, []):
        if start <= target <= end:
            return name
    return None


def receiver_chain(leaf: str, snippet: str) -> str | None:
    """The expression a member is reached through on this line, if any."""
    match = re.search(rf"(?<![A-Za-z0-9_])({re.escape(leaf)})(?![A-Za-z0-9_])", snippet)
    if match is None:
        return None
    prefix = snippet[: match.start()]
    chain = RECEIVER_CHAIN.search(prefix)
    if chain is None:
        return None
    text = chain.group(1).strip()
    return text[:-1].strip() if text.endswith(".") else text


def enclosing_literal(relative: str, line_number: int) -> str | None:
    """The type whose literal or pattern block a line sits inside.

    A field line says nothing about which literal it belongs to, and adjacent
    literals nest: `CompletedToolCall { call_id: ..., model_return: ModelToolReturn
    { call_id: ... } }` writes the same field name twice for two different types
    (FIG-1223).
    """
    lines = source_file_lines(relative)
    if lines is None:
        return None
    depth = 0
    stack: list[str | None] = []
    for index in range(line_number - 1):
        line = lines[index]
        for position, character in enumerate(line):
            if character == "{":
                head = line[:position].rstrip()
                name = re.search(r"([A-Za-z_][\w:]*)\s*$", head)
                candidate = name.group(1).split("::")[-1] if name else None
                stack.append(candidate if candidate and candidate[:1].isupper() else None)
            elif character == "}" and stack:
                stack.pop()
    for candidate in reversed(stack):
        if candidate:
            return candidate
    return None


def declaration_anchor_defect(symbol: str, reference: str) -> str | None:
    """Why an anchor is the item's own declaration rather than a use of it.

    `pub struct PreparedTurnMachine<..>` is where the type is *defined*; a row
    anchored there records that the workspace declares its own API, which every
    row could claim (FIG-1223).
    """
    segments = symbol.split("::")
    leaf = segments[-1]
    owner = next((segment for segment in segments if segment[:1].isupper()), None)
    if owner is None:
        return None
    if not leaf[:1].isupper():
        # A member is owned by its type, and the type is what a crate declares:
        # `lash_core::RuntimeError::message` is declared in `lashlang`, which the
        # leaf name alone never revealed.
        leaf = owner
    snippet = reference.split("#", 1)[-1]
    if re.search(rf"\b(?:struct|enum|trait|union|type)\s+{re.escape(leaf)}\b", snippet):
        return "declares the item rather than using it"
    location = anchor_location(reference)
    if location is None:
        return None
    declared = {name for _, _, name, _ in type_facts()["declarations"].get(location[0], [])}
    if re.search(rf"\w\s*::\s*{re.escape(leaf)}(?![A-Za-z0-9_])", snippet):
        # The line writes the path, so it is unambiguously about this item even
        # in a file that declares a wrapper by the same name.
        return None
    parts = location[0].split("/")
    crate = f"{parts[0]}/{parts[1]}" if len(parts) > 1 else parts[0]
    if crate in declaring_crates({leaf}):
        return (
            f"sits in {crate}, the crate whose source declares {leaf}; a crate "
            "citing its own declaration is not an internal consumer"
        )
    if leaf in declared:
        # The file that declares the type is where it lives, whatever path the
        # ledger keys it under: `lash_core::PreparedTurnMachine` is declared in
        # `lash-sansio`, so a signature there is the definition, not a consumer.
        return "sits in the file that declares the item, which defines it rather than consuming it"
    return None


def qualifying_container(owner: str, snippet: str) -> str | None:
    """The type segment a line qualifies the owner with, if it is a type.

    `TurnFinish::FinalValue { value }` and `TurnEvent::FinalValue { value }` write
    the same variant name under two different enums, and a variant is only as
    unique as the type it belongs to (FIG-1223).
    """
    match = re.search(
        rf"([A-Za-z_]\w*)\s*::\s*{re.escape(owner)}(?![A-Za-z0-9_])", snippet
    )
    if match is None:
        return None
    segment = match.group(1)
    return segment if segment[:1].isupper() else None


IMPORT_LINE = re.compile(r"^\s*(?:pub(?:\s*\([^)]*\))?\s+)?use\s")


IMPORT_MESSAGE = (
    "is an import, which proves the path resolves and not that anything needs "
    "it; anchor the line that uses the item"
)


def import_anchor_defect(reference: str) -> str | None:
    """Why an import cannot be an internal consumer's anchor.

    Ruled for FIG-1223: a `use` line is *reachability*, not consumption.  It
    resolves whether or not anything in the crate needs the item -- an unused
    import is a warning, not a contract -- and four rows anchored on `pub use`
    lines were citing the declaring crate's own re-export.  An internal seam's
    claim is that shipped code needs the item, so the anchor has to be a line
    that uses it: a call, a field, a literal, or an implementation of it.
    """
    if IMPORT_LINE.match(reference.split("#", 1)[-1]):
        return IMPORT_MESSAGE
    location = anchor_location(reference)
    if location is None:
        return None
    lines = source_file_lines(location[0]) or []
    # A `use lash_core::{` list runs down the page, and its middle lines look
    # like bare identifiers: four rows anchored on one.
    for index in range(location[1] - 2, max(-1, location[1] - 15), -1):
        text = lines[index].strip() if index < len(lines) else ""
        # A statement that ended is not a list this line is inside of, and that
        # has to be read before the `use` itself: a completed `use foo::bar;`
        # above real code otherwise swallows the code (44 lines here).  A `use`
        # ending in `{` is the exception, since that brace opens the list.
        if not text or text.endswith((";", "}")) or text.startswith("#["):
            break
        if IMPORT_LINE.match(text):
            return IMPORT_MESSAGE
        if text.endswith("{"):
            break
    return None


def declaring_crates(names: set[str]) -> set[str]:
    """Crate directories whose source declares any of these type names.

    The ledger keys an item by the path a host writes, which is often a facade
    re-export: `lash_core::PreparedTurnMachine` is declared in `lash-sansio`, and
    mapping only path roots let that crate cite its own declaration as if it were
    a consumer (FIG-1223).
    """
    declarations = type_facts()["declarations"]
    found: set[str] = set()
    for relative, entries in declarations.items():
        if not any(name in names for _, _, name, _ in entries):
            continue
        parts = relative.split("/")
        if len(parts) > 1:
            found.add(f"{parts[0]}/{parts[1]}")
    return found


def member_anchor_defect(
    symbol: str, aliases: Any, kind: str, rivals: set[str], reference: str
) -> str | None:
    """Why a member's anchor could be about something else entirely, or None.

    `reference_exists` is a substring test and a member's leaf name is rarely
    unique, so for the dispositions whose justification *is* the anchor the leaf
    alone cannot carry it: `SchemaDialect::as_str` was "proved" by `serde_json`'s
    `value.as_str()`, two unrelated members by the same `row.get(3)`, and a
    variant's field by a `tungstenite` import alias (FIG-1223).

    A line that *qualifies* the member by its owner settles it outright.  Failing
    that, a line that reaches the member through an expression is judged on the
    expression alone: it has to resolve, hop by hop, to the owning type or to a
    type that implements the owning trait.  Naming no rival is not a defence --
    prelude types, file-local types and path-qualified rivals are all unnameable,
    so an unresolved receiver is a defect whether or not a contender can be
    pointed at.  A field written in a literal is judged by the literal it sits
    in, because adjacent literals write the same field name for different types.
    Only anchors of neither shape fall back to reading the surrounding function.
    """
    owners = member_owners(symbol, aliases, kind)
    if not owners:
        return None
    containers = member_containers(symbol, aliases, kind)
    leaf = symbol.split("::")[-1]
    snippet = reference.split("#", 1)[-1]
    location = anchor_location(reference)
    lines = None if location is None else source_file_lines(location[0])
    if not lines:
        return "does not resolve to a readable file"
    relative, line_number = location
    qualified = [owner for owner in owners if qualifies_member(owner, leaf, snippet)]
    written = {
        segment
        for owner in qualified
        if (segment := qualifying_container(owner, snippet)) is not None
    }
    if written and not written & (owners | containers):
        # The line spells out a container, and it is somebody else's: a variant of
        # a different enum, not this row's member.
        return (
            f"qualifies {leaf} under {' or '.join(sorted(written))}, not under "
            f"{' or '.join(sorted(containers or owners))}, so it writes a "
            "same-named member of another type"
        )
    if qualified and (
        not containers
        or any(names_word(name, snippet) for name in containers)
        or any(names_word(name, anchor_scope(relative, line_number)) for name in containers)
    ):
        return None
    scope = anchor_scope(relative, line_number)
    declaration = declaration_owner(relative, line_number)
    if declaration is not None:
        return (
            f"sits inside the declaration of {declaration}, and a declaration is "
            "not a consumer: the line declares that crate's own same-named member"
        )
    prefix = expression_prefix(relative, line_number, leaf, snippet)
    assembled = RECEIVER_CHAIN.search(prefix)
    chain = None
    if assembled:
        text = assembled.group(1).strip()
        chain = re.sub(r"\s+", "", text[:-1].strip() if text.endswith(".") else text)
    if chain:
        resolved = resolve_receiver_types(relative, line_number, chain)
        implemented = {
            trait
            for candidate in resolved
            for trait in type_facts()["traits"].get(candidate, set())
        }
        if (resolved | implemented) & owners or binds_receiver(
            scope, chain.split(".")[-1].removesuffix("()"), owners
        ):
            return None
        return (
            f"reaches the member through `{chain}`, which resolves to "
            f"{' or '.join(sorted(resolved)) or 'no type this repository declares'}"
            f" rather than {' or '.join(sorted(owners))}, so the line is about "
            "some other type's same-named member"
        )
    if re.search(rf"Self\s*::\s*{re.escape(leaf)}(?![A-Za-z0-9_])", snippet) and (
        impl_owner(relative, line_number) & owners
        or {
            trait
            for candidate in impl_owner(relative, line_number)
            for trait in type_facts()["traits"].get(candidate, set())
        }
        & owners
    ):
        return None
    if re.search(rf"(?<![A-Za-z0-9_.]){re.escape(leaf)}\s*[:,}}]", snippet) or re.search(
        rf"(?<![A-Za-z0-9_.]){re.escape(leaf)}\s*$", snippet
    ):
        stack = literal_stack(relative, line_number)
        literal = stack[-1] if stack else None
        if literal in owners and (
            not containers
            or containers & set(stack)
            or any(names_word(name, scope) for name in containers)
        ):
            return None
        return (
            f"writes the field inside a {literal or 'nameless'} literal"
            + (f" (nested in {' > '.join(stack[:-1])})" if len(stack) > 1 else "")
            + f", which is not a {' or '.join(sorted(owners))} one"
            + (
                f" carrying {' or '.join(sorted(containers))}"
                if containers
                else ""
            )
            + ", so it is a same-named field of another type"
        )
    implemented = {
        trait for _, _, _, trait in type_facts()["impls"].get(relative, []) if trait
    }
    if re.search(rf"\bfn\s+{re.escape(leaf)}\s*[(<]", snippet):
        candidates = impl_owner(relative, line_number)
        reachable = candidates | implemented | {
            trait
            for candidate in candidates
            for trait in type_facts()["traits"].get(candidate, set())
        }
        if reachable & (owners | containers):
            return None
        return (
            f"declares {leaf} in an implementation of "
            f"{' or '.join(sorted(reachable)) or 'nothing this repository names'}, "
            f"not of {' or '.join(sorted(owners))}"
        )
    contenders = (rivals | imported_type_names(relative)) - owners - containers
    named = sorted(rival for rival in contenders if names_word(rival, scope))
    return (
        f"names {leaf} without reaching the member: nothing qualifies it by "
        f"{' or '.join(sorted(owners))}, no receiver carries it, no literal "
        "contains it, and no implementation declares it"
        + (
            f", while {', '.join(named[:3])} in scope offer the same name"
            if named
            else " -- a bare name is not evidence"
        )
    )


def relocation_key(symbol: str, kind: str) -> str:
    """The identity a removal verdict follows when a path changes.

    A verdict is about an item, and an item survives being re-exported
    somewhere else, so the tombstone cannot key on the full path it was written
    at.  It keys on the part that names the thing: the type, or the member and
    the type that owns it.
    """
    segments = symbol.split("::")
    wanted = 2 if kind in MEMBER_KINDS else 1
    return "::".join(segments[-wanted:])


def removal_verdict_errors(
    verdicts: list[dict[str, Any]],
    recorded: Any,
    by_api: dict[tuple[str, str], dict[str, Any]],
    items: list[ApiItem],
) -> list[str]:
    """Enforce that a removal verdict can only be discharged by a removal.

    `678d567bf` is the case this exists for.  126 items carried written
    `unused-remove` verdicts; the commit moved them behind a doc-hidden module,
    deleted their 444 rows, and shipped.  Nothing was removed and nothing said
    so, because the only record of the verdict was the row the same diff erased.

    So the verdict outlives the row.  Every `unused-remove` row leaves a
    `[[removal_verdict]]` tombstone, and a tombstone is discharged exactly two
    ways: the item is gone from the surface, or the row still says
    `unused-remove`.  Anything else -- the item reappearing at a different path,
    or the same path acquiring a friendlier disposition -- requires
    `superseded_by` naming the new verdict, in the same diff, with a reason.
    `removal_verdicts_recorded` pins the tombstone count so that deleting
    history is an edit a reviewer sees rather than a line that vanishes among
    thousands.
    """
    errors: list[str] = []
    keys = [(verdict.get("symbol", ""), verdict.get("kind", "")) for verdict in verdicts]
    if not isinstance(recorded, int) or recorded != len(keys):
        errors.append(
            f"removal_verdicts_recorded is {recorded!r} but the inventory holds "
            f"{len(keys)} removal verdicts; a verdict may be added or superseded, "
            "never dropped unnoticed"
        )
    if keys != sorted(keys):
        errors.append("removal verdicts must be sorted by symbol and kind")
    if len(set(keys)) != len(keys):
        duplicated = sorted({key for key in keys if keys.count(key) > 1})
        errors.append(
            "duplicate removal verdicts: "
            + ", ".join(f"{symbol} ({kind})" for symbol, kind in duplicated)
        )

    by_path: dict[tuple[str, str], ApiItem] = {}
    by_leaf: dict[tuple[str, str], list[ApiItem]] = {}
    for item in items:
        for path in item.paths:
            by_path[(path, item.kind)] = item
        for path in item.paths:
            by_leaf.setdefault(
                (relocation_key(path, item.kind), item.kind), []
            ).append(item)

    tombstoned = set(keys)
    for (symbol, kind), row in sorted(by_api.items()):
        if row.get("disposition") == "unused-remove" and (symbol, kind) not in tombstoned:
            errors.append(
                f"{symbol} ({kind}): an unused-remove row without a removal "
                "verdict. Record a [[removal_verdict]] tombstone so the verdict "
                "survives this row being edited away."
            )

    for verdict in verdicts:
        symbol = verdict.get("symbol", "")
        kind = verdict.get("kind", "")
        superseded = verdict.get("superseded_by")
        reason = verdict.get("reason", "")
        if superseded is not None:
            if superseded not in DISPOSITIONS:
                errors.append(
                    f"{symbol} ({kind}): unknown superseding disposition {superseded!r}"
                )
            elif superseded == "unused-remove":
                errors.append(
                    f"{symbol} ({kind}): superseded_by repeats unused-remove, which "
                    "supersedes nothing; drop it or name the new verdict"
                )
            if not reason.strip():
                errors.append(
                    f"{symbol} ({kind}): superseding a removal verdict requires a "
                    "reason saying what changed the answer"
                )
        item = by_path.get((symbol, kind))
        relocated: ApiItem | None = None
        if item is None:
            candidates = by_leaf.get((relocation_key(symbol, kind), kind), [])
            if not candidates:
                # Discharged the honest way: the item is gone.
                continue
            relocated = candidates[0]
            item = relocated
        row = by_api.get((item.primary, item.kind))
        disposition = (row or {}).get("disposition")
        if superseded is None and disposition != "unused-remove":
            # A path change with the verdict intact is not laundering: the
            # removal is still owed, at whatever path the item now uses. What
            # `678d567bf` did was change the answer while moving the item, and
            # that is what needs saying out loud.
            where = (
                f"it now appears as {relocated.primary} with disposition "
                f"{disposition!r}"
                if relocated is not None
                else f"its row now reads {disposition!r}"
            )
            errors.append(
                f"{symbol} ({kind}): a removal verdict was written for this item, "
                f"and {where}. Relocation does not discharge a removal verdict "
                "(FIG-1223): remove the item, keep the verdict, or record "
                "superseded_by with the reason the verdict no longer holds."
            )
        elif superseded is not None and superseded != disposition:
            errors.append(
                f"{symbol} ({kind}): superseded_by says {superseded!r} but the "
                f"row at {item.primary} reads {disposition!r}; the tombstone and "
                "the ledger have to agree"
            )
    return errors


def internal_consumer_errors(
    by_api: dict[tuple[str, str], dict[str, Any]],
    items: list[ApiItem],
    crate_dirs: dict[str, str],
) -> list[str]:
    """Reject an `internal-consumed` anchor that sits in the defining crate.

    The claim is that *another* crate's shipped code needs the item, so the
    crates that own it prove nothing: a crate using its own code is not a seam.
    Ownership comes from the item's compiler identity as well as its paths --
    `lash::X` is usually defined in `lash-core`, and reading only the path would
    let the definition site pass for a consumer.
    """
    errors: list[str] = []
    for item in items:
        row = by_api.get((item.primary, item.kind))
        if row is None or row.get("disposition") != "internal-consumed":
            continue
        owners = {
            directory
            for path in [item.identity, *item.paths]
            if (directory := crate_dirs.get(path.split("::", 1)[0])) is not None
        }
        consumer = anchor_crate(row.get("usage", ""))
        if consumer is not None and consumer in owners:
            errors.append(
                f"{item.primary}: internal-consumed anchor "
                f"{row.get('usage')!r} points into {consumer}, which defines "
                "this item. A consumer is another crate's src/; a crate using "
                "its own code proves nothing about the seam."
            )
    return errors


def tier_breakdown(entries: list[dict[str, Any]]) -> list[str]:
    """The standing evidence distribution, printed on every run.

    A number nobody sees is a number nobody fixes: 84% of this inventory's
    example coverage turned out to be the examples' own tests, and it stayed
    invisible for as long as the report only said "passed".
    """
    per_tier: dict[str, int] = {tier: 0 for tier in ANCHOR_TIERS}
    unresolved = 0
    per_disposition: dict[str, dict[str, int]] = {}
    for entry in entries:
        usage = entry.get("usage", "")
        if not usage:
            continue
        tier = anchor_tier(usage)
        if tier is None:
            unresolved += 1
            continue
        per_tier[tier] += 1
        per_disposition.setdefault(entry.get("disposition", ""), {}).setdefault(tier, 0)
        per_disposition[entry.get("disposition", "")][tier] += 1
    anchored = sum(per_tier.values())
    lines = ["evidence tiers (rows by usage anchor):"]
    for tier in ANCHOR_TIERS:
        count = per_tier[tier]
        share = f"{100 * count / anchored:.0f}%" if anchored else "0%"
        lines.append(f"- {tier}: {count} ({share})")
    if unresolved:
        lines.append(f"- unresolved anchors: {unresolved}")
    for disposition in ANCHORED_DISPOSITIONS:
        tiers = per_disposition.get(disposition)
        if not tiers:
            continue
        detail = ", ".join(
            f"{tier} {tiers[tier]}" for tier in ANCHOR_TIERS if tiers.get(tier)
        )
        lines.append(f"- {disposition}: {detail}")
    ratcheted = sum(
        counts.get("example-test", 0)
        for disposition, counts in per_disposition.items()
        if disposition.startswith("used-")
    )
    lines.append(
        f"- example-test ratchet: {ratcheted} of {EXAMPLE_TEST_TIER_RATCHET} "
        "example-coverage rows"
    )
    return lines


def example_test_tier_errors(entries: list[dict[str, Any]]) -> list[str]:
    """The ratchet on example-test evidence: it may fall, never rise.

    Each of these rows is a per-row design question -- should the example's host
    code use this item, or is a test the honest home for it? -- so they are not
    migrated in bulk.  What they must not do is grow, which is why the gate holds
    a number instead of a note.
    """
    count = sum(
        1
        for entry in entries
        if entry.get("disposition", "").startswith("used-")
        and entry.get("usage")
        and anchor_tier(entry["usage"]) == "example-test"
    )
    if count > EXAMPLE_TEST_TIER_RATCHET:
        return [
            f"{count} rows anchor example coverage in an example's own tests, "
            f"above the {EXAMPLE_TEST_TIER_RATCHET}-row ratchet. Anchor the new "
            "row in the example's host code, or re-disposition it."
        ]
    if count < EXAMPLE_TEST_TIER_RATCHET:
        return [
            f"only {count} rows anchor example coverage in an example's own "
            f"tests, below the {EXAMPLE_TEST_TIER_RATCHET}-row ratchet. Lower "
            "EXAMPLE_TEST_TIER_RATCHET to that number so the ratchet keeps its "
            "teeth."
        ]
    return []


def item_errors(
    by_api: dict[tuple[str, str], dict[str, Any]], items: list[ApiItem]
) -> list[str]:
    """Reconcile the inventory against the compiler's API items.

    One item is one row.  Rows found under several of an item's paths are the
    failure this exists to catch: the same function was `unused-add` through the
    facade and `unused-remove` through `lash_core`, which is two answers to one
    contract question.  The contradiction is named explicitly because its fix is
    a decision, while a mere repeat of the same disposition just needs the alias
    row dropped.

    Centralizing the *verdict* must not decentralize *existence*: the row's
    `aliases` are compared against the compiler's projection, so retiring or
    adding a `lash_core::` re-export -- breaking for direct core consumers under
    ADR 0051 -- still fails the gate even though it changes no disposition.
    """
    errors: list[str] = []
    matched: set[tuple[str, str]] = set()
    for item in items:
        primary, kind = item.primary, item.kind
        rows = [(path, by_api[(path, kind)]) for path in item.paths if (path, kind) in by_api]
        matched.update((path, kind) for path, _ in rows)
        if not rows:
            errors.append(f"undispositioned public API: {primary} ({kind})")
            continue
        if len(rows) > 1:
            detail = ", ".join(
                f"{alias} = {row.get('disposition')!r}" for alias, row in rows
            )
            if len({row.get("disposition") for _, row in rows}) > 1:
                errors.append(
                    f"{primary} ({kind}): contradictory dispositions for one API "
                    f"item: {detail}. One item carries one disposition; keep the "
                    f"row at {primary} and delete the others."
                )
            else:
                errors.append(
                    f"{primary} ({kind}): one API item recorded under several of "
                    f"its paths: {detail}. An alias belongs in this item's row at "
                    f"{primary} as an alias, not as a second row."
                )
            continue
        recorded_path, row = rows[0]
        if recorded_path != primary:
            errors.append(
                f"{primary} ({kind}): recorded at the alias path {recorded_path}; "
                f"record each API item at its primary path"
            )
        if row.get("availability") != item.availability:
            errors.append(
                f"{primary} ({kind}): availability changed from "
                f"{row.get('availability')!r} to {item.availability!r}"
            )
        recorded_aliases = row.get("aliases") or []
        if sorted(recorded_aliases) != item.aliases():
            retired = sorted(set(recorded_aliases) - set(item.aliases()))
            added = sorted(set(item.aliases()) - set(recorded_aliases))
            detail = []
            if retired:
                detail.append(
                    f"no longer public: {', '.join(retired)} -- retiring a path is "
                    f"breaking for its direct consumers (ADR 0051)"
                )
            if added:
                detail.append(
                    f"newly public: {', '.join(added)} -- a new path is a new promise"
                )
            errors.append(
                f"{primary} ({kind}): the item's public paths changed. "
                f"{'; '.join(detail)}. Record the change in this row's aliases."
            )
    for symbol, kind in sorted(set(by_api) - matched):
        errors.append(f"inventory names an API that is no longer public: {symbol} ({kind})")
    return errors


def row_errors(
    entry: dict[str, Any],
    *,
    table: InventoryTable,
    seen: dict[tuple[str, str], dict[str, Any]],
    leaf_owners: dict[str, set[str]],
    facade_dirs: set[str],
) -> list[str]:
    """Every check one inventory row answers to, whichever table holds it.

    `seen` is the table's own symbol mapping, carried in so the row can be
    checked against the rows already read: a second row for one symbol is an
    invalid state in both tables, and keeping the population in a set is what
    let two low-level rows for one symbol validate independently.  The caller
    reuses the `api` mapping afterwards for the surface reconciliation.
    """
    errors: list[str] = []
    symbol = entry.get("symbol", "")
    kind = entry.get("kind", "")
    label = f"{symbol} ({kind})" if table.states("kind") else symbol
    key = (symbol, kind)
    if key in seen:
        errors.append(f"duplicate inventory entry: {label}")
    seen[key] = entry
    if table.states("availability"):
        availability = entry.get("availability", "")
        if availability not in {"default", "all-features", "default+all-features"}:
            errors.append(f"{symbol}: invalid availability {availability!r}")
    if table.states("aliases"):
        aliases = entry.get("aliases", [])
        if not isinstance(aliases, list) or not all(
            isinstance(alias, str) for alias in aliases
        ):
            errors.append(f"{symbol}: aliases must be a list of paths")
        elif aliases != sorted(aliases) or symbol in aliases or len(set(aliases)) != len(aliases):
            errors.append(
                f"{symbol}: aliases must be sorted, distinct, and exclude the primary path"
            )
        elif "aliases" in entry and not aliases:
            errors.append(f"{symbol}: omit aliases rather than recording an empty list")
    if table.states("area") and entry.get("area") not in AREAS:
        errors.append(f"{symbol}: unknown area {entry.get('area')!r}")
    disposition = entry.get("disposition")
    if disposition not in DISPOSITIONS:
        errors.append(f"{symbol}: unknown disposition {disposition!r}")
    usage = entry.get("usage", "")
    assertion = entry.get("assertion", "")
    if disposition in {"used-asserted", "used-unasserted"}:
        if not reference_exists(usage):
            errors.append(f"{symbol}: stale or invalid example usage reference {usage!r}")
        else:
            defect = perfunctory_exercise(
                symbol,
                kind,
                anchored_exercise_source(usage) or "",
                disposition or "",
            )
            if defect:
                errors.append(
                    f"{symbol}: example usage anchor {usage!r} {defect}. "
                    "Anchor an executed operation or observed outcome instead."
                )
    if disposition == "used-asserted":
        if not reference_exists(assertion):
            errors.append(
                f"{symbol}: stale or invalid example assertion reference {assertion!r}"
            )
        elif tautological_assertion(assertion):
            errors.append(
                f"{symbol}: assertion anchor {assertion!r} is a tautology -- "
                "size_of/align_of holds for every non-ZST and proves only that "
                "the path resolves. Assert an outcome the runtime produced."
            )
        else:
            defect = uninformative_assertion(
                assertion, anchored_source(assertion) or ""
            )
            if defect:
                errors.append(
                    f"{symbol}: assertion anchor {assertion!r} {defect}. "
                    "Anchor the line that states what the example observed."
                )
            inherited = (
                None
                if usage == assertion
                else unrelated_fluent_assertion(
                    anchored_source(usage) or "",
                    anchored_source(assertion) or "",
                )
            )
            if inherited:
                errors.append(
                    f"{symbol}: fluent usage anchor {usage!r} {inherited}; "
                    f"assertion {assertion!r} does not establish this call's outcome. "
                    "Anchor a direct outcome or downgrade the disposition."
                )
    elif assertion:
        errors.append(f"{symbol}: only used-asserted entries may name an assertion")
    if disposition in INTERNAL_DISPOSITIONS:
        resolved_usage = resolved_internal_reference(usage)
        if resolved_usage is None:
            errors.append(
                f"{symbol}: stale or invalid internal consumer reference "
                f"{usage!r}. An internal seam's justification is its anchor, "
                "so the recorded file and source text have to resolve."
            )
        checked_usage = resolved_usage or usage
        imported = import_anchor_defect(checked_usage)
        if imported:
            errors.append(
                f"{symbol}: usage anchor {usage!r} {imported}. An internal "
                "seam's claim is that shipped code needs the item."
            )
        declared = declaration_anchor_defect(symbol, checked_usage)
        if declared:
            errors.append(
                f"{symbol}: usage anchor {usage!r} {declared}. An internal "
                "consumer's anchor is a line that needs the item, not the "
                "line that defines it."
            )
        if table.states("kind") and table.states("aliases"):
            owners = member_owners(symbol, entry.get("aliases"), kind or "")
            defect = member_anchor_defect(
                symbol,
                entry.get("aliases"),
                kind or "",
                leaf_owners.get(symbol.split("::")[-1], set()) - owners,
                checked_usage,
            )
            if defect:
                errors.append(
                    f"{symbol}: usage anchor {usage!r} {defect}. Anchor a line "
                    "that names the owning type, or record the consumer in prose "
                    "under a disposition that does not claim machine-checked "
                    "evidence."
                )
        if disposition == "internal-test-only":
            reason_text = entry.get("reason", "")
            if not reason_text.strip():
                errors.append(
                    f"{symbol}: internal-test-only requires a reason; the anchor "
                    "says a test consumes it, not why that is the whole story"
                )
            elif (
                not any(home in symbol for home in FEATURE_GATED_TEST_HOMES)
                and "Relocate:" not in reason_text
            ):
                errors.append(
                    f"{symbol}: internal-test-only outside a testing module "
                    "requires a Relocate: note naming where the item goes, "
                    "because a test is not a home"
                )
    if disposition.startswith("unused-") and usage:
        errors.append(
            f"{symbol}: an unused disposition may not name a usage anchor "
            f"({usage!r}); an anchor is a claim of use"
        )
    for field, reference in (("usage", usage), ("assertion", assertion)):
        allowed = DISPOSITION_TIERS.get(disposition or "")
        if not reference or allowed is None:
            continue
        tier = anchor_tier(reference)
        if tier not in allowed:
            errors.append(
                f"{symbol}: {field} anchor {reference!r} is "
                f"{tier or 'unrecognized'}-tier evidence, but {disposition} "
                f"anchors in {' or '.join(sorted(allowed))}. Each tier proves "
                "a different thing; a row may not blend them."
            )
    if disposition.startswith("unused-") and not entry.get("reason", "").strip():
        errors.append(f"{symbol}: unused disposition requires a concrete reason")
    if disposition == "unused-remove" and "Breaking:" not in entry.get("reason", ""):
        errors.append(f"{symbol}: removal disposition requires a Breaking: note")
    reason = entry.get("reason", "")
    # The prose lints read the row's claims, not the source it quotes: a
    # citation's snippet is code, and code may legitimately say `/`.
    prose = quoted_prose(reason)
    for field, value in (("reason", prose), ("usage", usage), ("assertion", assertion)):
        # For evidence references only the location is a path; the anchored
        # source text after `#` is code, and code may legitimately say `/`.
        offender = machine_local_path(
            value if field == "reason" else value.split("#", 1)[0]
        )
        if offender:
            errors.append(
                f"{symbol}: {field} names the machine-local path {offender!r}; "
                "evidence must be repository-relative"
            )
    citation = prose_citation_defect(
        symbol, kind or "", reason, anchor_locations(entry)
    )
    if citation:
        errors.append(
            f"{symbol}: reason {citation}. A citation a reader cannot "
            "confirm is not a justification."
        )
    stale = stale_disposition_reason(disposition or "", prose)
    if stale:
        errors.append(f"{symbol}: reason {stale}")
    missing = missing_repository_path(prose)
    if missing:
        errors.append(
            f"{symbol}: reason cites missing repository file {missing!r}; "
            "cited evidence paths must exist"
        )
    migration = impossible_facade_migration(prose, facade_dirs)
    if migration:
        errors.append(
            f"{symbol}: reason parks the consumer at {migration!r} behind a "
            "migration to the lash facade, but the facade is built on that "
            "crate and it cannot depend on the facade"
        )
    return errors


def check() -> int:
    document = inventory_document()
    entries = document.get("api", [])
    by_api: dict[tuple[str, str], dict[str, Any]] = {}
    errors: list[str] = []
    facade_dirs = facade_dependency_dirs()
    crate_dirs = crate_directories()
    leaf_owners = member_leaf_owners(entries)
    low_level_entries = document.get("low_level_api", [])
    low_level_symbols = {entry.get("symbol", "") for entry in low_level_entries}
    missing_low_level = sorted(REQUIRED_LOW_LEVEL_API - low_level_symbols)
    unexpected_low_level = sorted(low_level_symbols - REQUIRED_LOW_LEVEL_API)
    if missing_low_level:
        errors.append(
            "undispositioned low-level Lashlang API: " + ", ".join(missing_low_level)
        )
    if unexpected_low_level:
        errors.append(
            "unknown low-level Lashlang API disposition: "
            + ", ".join(unexpected_low_level)
        )
    low_level_seen: dict[tuple[str, str], dict[str, Any]] = {}
    for entry in low_level_entries:
        errors += row_errors(
            entry,
            table=LOW_LEVEL_TABLE,
            seen=low_level_seen,
            leaf_owners=leaf_owners,
            facade_dirs=facade_dirs,
        )
    for entry in entries:
        errors += row_errors(
            entry,
            table=API_TABLE,
            seen=by_api,
            leaf_owners=leaf_owners,
            facade_dirs=facade_dirs,
        )

    all_entries = [*entries, *low_level_entries]
    errors += example_test_tier_errors(all_entries)
    # A citation only answers to the check above while it carries a line: the
    # pattern reads `file.rs:line` and nothing else, so deleting `:1079` turns a
    # checked claim into unreadable prose.  That is how 24 rows left the check in
    # the first FIG-1526 round, and pinning the population makes the next one an
    # edit a reviewer sees.
    counted_citations = prose_citations(all_entries)
    recorded_citations = document.get("prose_citations_recorded")
    if not isinstance(recorded_citations, int) or recorded_citations != counted_citations:
        errors.append(
            f"prose_citations_recorded is {recorded_citations!r} but the reasons "
            f"hold {counted_citations} symbol citations; re-anchor a citation "
            "rather than dropping it, and move the pin when the population really "
            "changes"
        )

    items = current_surface()
    errors += item_errors(by_api, items)
    errors += internal_consumer_errors(by_api, items, crate_dirs)
    verdicts = document.get("removal_verdict", [])
    errors += removal_verdict_errors(
        verdicts, document.get("removal_verdicts_recorded"), by_api, items
    )

    for line in tier_breakdown(all_entries):
        print(line)
    print(f"removal verdicts recorded: {len(verdicts)}")
    if errors:
        print("API example-coverage contract failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(f"API example-coverage contract passed ({len(items)} entries)")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--dump-surface",
        action="store_true",
        help="print the compiler-derived surface as JSON instead of checking",
    )
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="re-anchor stale evidence references whose source text still exists, then exit",
    )
    args = parser.parse_args()
    if args.dump_surface:
        return dump_surface()
    if args.refresh:
        return refresh()
    return check()


if __name__ == "__main__":
    raise SystemExit(main())
