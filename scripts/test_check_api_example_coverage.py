#!/usr/bin/env python3

import contextlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import check_api_example_coverage
from check_api_example_coverage import (
    ApiItem,
    check,
    EXAMPLE_TEST_TIER_RATCHET,
    _IMPORTED_TYPES,
    _SCOPE_BLOCKS,
    _SOURCE_LINES,
    _ANCHOR_SCOPES,
    _CONSUMING_LINES,
    _RESOLVED_RECEIVERS,
    _LITERAL_STACKS,
    _TYPE_FACTS,
    anchor_crate,
    anchor_scope,
    anchor_tier,
    api_items,
    binds_receiver,
    cfg_gates_test,
    evaluate_cfg,
    feature_gated_test_home,
    parse_cfg,
    declared_test_modules,
    doc_hidden,
    example_test_tier_errors,
    internal_consumer_errors,
    relocation_key,
    resolved_internal_reference,
    removal_verdict_errors,
    scope_blocks,
    test_module_paths,
    test_path,
    test_regions,
    tier_breakdown,
    impossible_facade_migration,
    item_errors,
    lash_core_surface,
    machine_local_path,
    declaration_anchor_defect,
    declaring_crates,
    import_anchor_defect,
    member_anchor_defect,
    member_containers,
    member_leaf_owners,
    member_owners,
    prose_citation_defect,
    _SYMBOL_BLOCKS,
    qualifies_member,
    missing_repository_path,
    module_directory,
    perfunctory_exercise,
    primary_path,
    stale_disposition_reason,
    tautological_assertion,
    unrelated_fluent_assertion,
    uninformative_assertion,
)


def item(name, visibility, inner, item_id, **extra):
    return {
        "id": item_id,
        "name": name,
        "visibility": visibility,
        "inner": inner,
        **extra,
    }


def resolved(item_id):
    return {"resolved_path": {"path": "Ignored", "id": item_id, "args": None}}


def sig(output=None, inputs=()):
    return {"sig": {"inputs": list(inputs), "output": output, "is_c_variadic": False}}


def fixture():
    """A root export that hands out a `pub(crate)`-rooted type.

    `Handle` is the only named export. `Handed` is reachable because
    `Handle::handed` returns it, yet its module is crate-private, so no module
    walk can ever name it. `Hidden` is doc-hidden, and `Unreached` lives in a
    public module nothing reachable mentions.
    """
    return {
        "root": 0,
        "index": {
            "0": item("lash_core", "public", {"module": {"items": [1, 20, 30]}}, 0),
            # ── root export ──
            "1": item(
                "Handle",
                "public",
                {"use": {"id": 2, "source": "h::Handle", "is_glob": False}},
                1,
            ),
            "2": item("Handle", "public", {"struct": {"kind": "unit", "impls": [3]}}, 2),
            "3": item(None, "public", {"impl": {"trait": None, "items": [4, 5]}}, 3),
            "4": item("handed", "public", {"function": sig(resolved(6))}, 4),
            "5": item(
                "hidden_handed",
                "public",
                {"function": sig(resolved(10))},
                5,
                attrs=["#[doc(hidden)]"],
            ),
            # ── reachable-only type, plus its own members ──
            "6": item(
                "Handed",
                "public",
                {"struct": {"kind": {"plain": {"fields": [8]}}, "impls": [7]}},
                6,
            ),
            "7": item(None, "public", {"impl": {"trait": None, "items": [9]}}, 7),
            "8": item("slot", "public", {"struct_field": {"primitive": "u64"}}, 8),
            "9": item("callable", "public", {"function": sig()}, 9),
            # ── doc-hidden type, returned only by a doc-hidden member ──
            "10": item("Hidden", "public", {"struct": {"kind": "unit", "impls": []}}, 10),
            # ── public module nothing reachable mentions ──
            "20": item("internals", "public", {"module": {"items": [21]}}, 20),
            "21": item("Unreached", "public", {"struct": {"kind": "unit", "impls": []}}, 21),
            # ── doc-hidden support module: the internal cross-crate API ──
            "30": item(
                "support",
                "public",
                {"module": {"items": [31]}},
                30,
                attrs=["#[doc(hidden)]"],
            ),
            "31": item("Bridged", "public", {"struct": {"kind": "unit", "impls": []}}, 31),
        },
        "paths": {
            # `Handle` is exported as `lash_core::Handle` but defined in `h`, so
            # its identity is the definition path, not the export path.
            "2": {"path": ["lash_core", "h", "Handle"], "kind": "struct"},
            "6": {"path": ["lash_core", "private_mod", "Handed"], "kind": "struct"},
            "10": {"path": ["lash_core", "private_mod", "Hidden"], "kind": "struct"},
            "31": {"path": ["lash_core", "bridge", "Bridged"], "kind": "struct"},
        },
    }


class DocHiddenTests(unittest.TestCase):
    def test_recognizes_every_recorded_attribute_shape(self):
        self.assertTrue(doc_hidden({"attrs": ["#[doc(hidden)]"]}))
        self.assertTrue(doc_hidden({"attrs": ["#[doc( hidden )]"]}))
        self.assertTrue(doc_hidden({"attrs": [{"doc_hidden": None}]}))
        self.assertFalse(doc_hidden({"attrs": ["#[doc = \"hidden costs\"]"]}))
        self.assertFalse(doc_hidden({}))

    def test_recognizes_the_unparsed_wrapper_this_rustdoc_emits(self):
        # The shape that made the gated-module guard answer "no" to every item,
        # including facade_support itself: the attribute is a *value*, not a key.
        self.assertTrue(doc_hidden({"attrs": [{"other": "#[doc(hidden)]"}]}))
        self.assertFalse(doc_hidden({"attrs": [{"other": "#[inline]"}]}))
        self.assertFalse(doc_hidden({"attrs": [{"other": None}]}))


class CoreSurfaceTests(unittest.TestCase):
    def setUp(self):
        self.surface = lash_core_surface(fixture(), False, {"support"})

    def test_names_the_root_export_and_its_members(self):
        self.assertIn(("lash_core::Handle", "struct"), self.surface)
        self.assertIn(("lash_core::Handle::handed", "function"), self.surface)

    def test_enumerates_a_pub_crate_rooted_reachable_type_by_canonical_path(self):
        # FIG-937: the Session-class hole. `Handed` has no nameable path, but a
        # host holds the value and can call everything on it.
        self.assertIn(("lash_core::private_mod::Handed", "struct"), self.surface)
        self.assertIn(("lash_core::private_mod::Handed::slot", "field"), self.surface)
        self.assertIn(("lash_core::private_mod::Handed::callable", "function"), self.surface)

    def test_includes_doc_hidden_members_and_what_they_expose(self):
        # FIG-1223: hidden is a documentation choice, not a ledger exemption. A
        # doc-hidden member and the type only it hands out both carry rows.
        self.assertIn(("lash_core::Handle::hidden_handed", "function"), self.surface)
        self.assertIn(("lash_core::private_mod::Hidden", "struct"), self.surface)

    def test_walks_a_gated_doc_hidden_support_module(self):
        self.assertIn(("lash_core::support::Bridged", "struct"), self.surface)
        self.assertEqual(
            self.surface[("lash_core::support::Bridged", "struct")],
            "lash_core::bridge::Bridged",
        )

    def test_rejects_a_doc_hidden_root_module_missing_from_the_gated_list(self):
        # The amnesty channel: 304 paths that no row answered for, because one
        # attribute took them out of the gate entirely.
        with self.assertRaises(AssertionError) as raised:
            lash_core_surface(fixture(), False)
        self.assertIn("support", str(raised.exception))
        self.assertIn("gated_core_modules", str(raised.exception))

    def test_rejects_a_gated_module_the_crate_no_longer_exports(self):
        # Judged against the all-features document: a module missing from *that*
        # pass is missing everywhere, which is what retirement means.
        with self.assertRaises(AssertionError) as raised:
            lash_core_surface(fixture(), True, {"support", "retired_support"})
        self.assertIn("retired_support", str(raised.exception))

    def test_tolerates_a_feature_gated_gated_module_absent_by_default(self):
        # FIG-1223: `test_support` is `#[cfg(any(test, feature = "testing"))]`,
        # so the default-features pass never sees it. Absent-without-the-feature
        # is what that module is supposed to be, not a retirement, and only the
        # all-features pass answers for existence.
        surface = lash_core_surface(fixture(), False, {"support", "test_support"})
        self.assertIn(("lash_core::support::Bridged", "struct"), surface)

    def test_excludes_unreached_public_module_internals(self):
        self.assertEqual(
            [symbol for symbol, _ in self.surface if "Unreached" in symbol], []
        )

    def test_keys_every_path_by_its_canonical_identity(self):
        # FIG-955: identity is what makes two paths one contract item.
        self.assertEqual(
            self.surface[("lash_core::Handle", "struct")], "lash_core::h::Handle"
        )
        self.assertEqual(
            self.surface[("lash_core::Handle::handed", "function")],
            "lash_core::h::Handle::handed",
        )


class ApiItemTests(unittest.TestCase):
    def test_paths_sharing_an_identity_are_one_item(self):
        surface = {
            ("lash::Session", "struct"): "lash_core::session::Session",
            ("lash::prelude::Session", "struct"): "lash_core::session::Session",
            ("lash_core::Session", "struct"): "lash_core::session::Session",
            ("lash::Other", "struct"): "lash_core::other::Other",
        }
        grouped = api_items(surface)
        self.assertEqual(
            grouped[("lash_core::session::Session", "struct")],
            ["lash::Session", "lash::prelude::Session", "lash_core::Session"],
        )
        self.assertEqual(grouped[("lash_core::other::Other", "struct")], ["lash::Other"])

    def test_primary_path_prefers_the_facade_then_the_shortest(self):
        self.assertEqual(
            primary_path(["lash_core::Session", "lash::sessions::Session"]),
            "lash::sessions::Session",
        )
        self.assertEqual(
            primary_path(["lash::sessions::Session", "lash::Session"]), "lash::Session"
        )
        self.assertEqual(
            primary_path(["lash_core::b::Thing", "lash_core::a::Thing"]),
            "lash_core::a::Thing",
        )


def row(symbol, disposition, availability="default+all-features", aliases=None):
    entry = {
        "symbol": symbol,
        "kind": "function",
        "availability": availability,
        "area": "sessions-turns",
        "disposition": disposition,
    }
    if aliases:
        entry["aliases"] = sorted(aliases)
    return entry


class OneDispositionPerItemTests(unittest.TestCase):
    """FIG-955 / audit item 4: one item carries one disposition.

    The pair below is the real contradiction #251 had to reconcile by hand --
    `AwaitEventKey::key_id` was `unused-add` through the facade and
    `unused-remove` through `lash_core`, and the gate passed. Both dispositions
    were individually well-formed, so nothing short of comparing them across an
    item's paths can catch it.
    """

    ITEM = ApiItem(
        primary="lash::AwaitEventKey::key_id",
        kind="function",
        availability="default+all-features",
        paths=["lash::AwaitEventKey::key_id", "lash_core::AwaitEventKey::key_id"],
        identity="lash_core::runtime::await_event::AwaitEventKey::key_id",
    )

    #: The alias set a correct row for `ITEM` records.
    ALIASES = ["lash_core::AwaitEventKey::key_id"]

    def errors(self, rows):
        return item_errors({(entry["symbol"], "function"): entry for entry in rows}, [self.ITEM])

    def primary(self, **kwargs):
        return row("lash::AwaitEventKey::key_id", "unused-add", aliases=self.ALIASES, **kwargs)

    def test_rejects_contradictory_dispositions_across_an_items_paths(self):
        errors = self.errors(
            [
                self.primary(),
                row("lash_core::AwaitEventKey::key_id", "unused-remove"),
            ]
        )
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("contradictory dispositions", errors[0])
        self.assertIn("'unused-add'", errors[0])
        self.assertIn("'unused-remove'", errors[0])

    def test_reports_agreeing_repeats_as_undropped_aliases_not_contradictions(self):
        errors = self.errors(
            [
                self.primary(),
                row("lash_core::AwaitEventKey::key_id", "unused-add"),
            ]
        )
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("recorded under several of its paths", errors[0])
        self.assertNotIn("contradictory", errors[0])

    def test_accepts_one_row_at_the_primary_path(self):
        self.assertEqual(self.errors([self.primary()]), [])

    def test_rejects_a_row_recorded_only_at_an_alias_path(self):
        # Misplacement necessarily also misstates the projection, so both rules
        # fire; the placement error is the one that names the fix.
        errors = self.errors([row("lash_core::AwaitEventKey::key_id", "unused-add")])
        self.assertIn("recorded at the alias path", errors[0])
        self.assertEqual(len(errors), 2, errors)

    def test_reports_an_item_with_no_row_and_a_row_with_no_item(self):
        errors = item_errors({("lash::Gone", "function"): row("lash::Gone", "unused-add")}, [self.ITEM])
        self.assertEqual(len(errors), 2, errors)
        self.assertIn("undispositioned public API: lash::AwaitEventKey::key_id", errors[0])
        self.assertIn("no longer public: lash::Gone", errors[1])

    def test_compares_availability_against_the_item_not_the_path(self):
        errors = self.errors([self.primary(availability="all-features")])
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("availability changed", errors[0])


class AliasExistenceGateTests(unittest.TestCase):
    """Centralizing the verdict must not decentralize existence.

    Collapsing a facade twin onto one row removes the row whose disappearance
    used to signal that a `lash_core::` re-export had been retired -- a change
    ADR 0051 makes breaking for direct core consumers. These are the reviewer's
    two probes: internalize a twin's core path, and invent one. Both changed the
    public path set while changing no disposition, and both must fail.
    """

    ITEM = ApiItem(
        primary="lash::AwaitEventKey::key_id",
        kind="function",
        availability="default+all-features",
        paths=[
            "lash::AwaitEventKey::key_id",
            "lash::prelude::AwaitEventKey::key_id",
            "lash_core::AwaitEventKey::key_id",
        ],
        identity="lash_core::runtime::await_event::AwaitEventKey::key_id",
    )

    def errors(self, aliases):
        entry = row("lash::AwaitEventKey::key_id", "unused-add", aliases=aliases)
        return item_errors({("lash::AwaitEventKey::key_id", "function"): entry}, [self.ITEM])

    def test_accepts_the_recorded_projection(self):
        self.assertEqual(self.errors(self.ITEM.aliases()), [])

    def test_rejects_a_recorded_path_the_compiler_no_longer_publishes(self):
        # Internalizing a `lash_core::` re-export: the row still promises it.
        # This is the direction ADR 0051 calls breaking, so the error says so.
        errors = self.errors(sorted(self.ITEM.aliases() + ["lash_core::Retired::key_id"]))
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("no longer public: lash_core::Retired::key_id", errors[0])
        self.assertIn("ADR 0051", errors[0])

    def test_rejects_a_public_path_the_row_does_not_record(self):
        # A new re-export is a new promise, and must be acknowledged.
        errors = self.errors(["lash::prelude::AwaitEventKey::key_id"])
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("newly public: lash_core::AwaitEventKey::key_id", errors[0])
        self.assertIn("a new path is a new promise", errors[0])

    def test_rejects_dropping_a_prelude_alias(self):
        errors = self.errors(["lash_core::AwaitEventKey::key_id"])
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("newly public: lash::prelude::AwaitEventKey::key_id", errors[0])


class TautologicalAssertionTests(unittest.TestCase):
    #: The two anchors the review found holding up a reversed removal verdict.
    ANCHORS = (
        "examples/agent-workbench/src/main_sections/tests/facade_homes.rs:63"
        "#assert!(std::mem::size_of::<lash::triggers::TriggerIngressReceipt>() > 0);",
        "examples/agent-workbench/src/main_sections/tests/facade_homes.rs:64"
        "#std::mem::align_of::<lash::triggers::TriggerDeliveryRetentionCandidate>()",
    )

    def test_rejects_size_of_and_align_of_anchors(self):
        for anchor in self.ANCHORS:
            self.assertTrue(tautological_assertion(anchor), anchor)

    def test_accepts_an_assertion_on_a_produced_outcome(self):
        self.assertFalse(
            tautological_assertion(
                "examples/docs-snippets/src/architecture_providers.rs:178"
                '#assert_eq!(capability_wire["reasoning"]["disable"]["budget"], 0);'
            )
        )

    def test_ignores_the_location_and_reads_only_the_anchored_source(self):
        # A path may legitimately contain the word; the anchor text may not.
        self.assertFalse(tautological_assertion("examples/size_of/src/main.rs:1#assert!(ok);"))


class UninformativeAssertionTests(unittest.TestCase):
    """FIG-970: an anchor is evidence only if its line says what was observed.

    Every opener below shipped as an `assertion` anchor on FIG-955's inventory --
    388 rows whose recorded evidence was the word `assert_eq!(` and a line number.
    `assert!(matches!(` is the shape the tautology lint could not see: two macro
    names, no operands, and a `matches!` pattern the reader never gets to check.
    """

    OPENERS = (
        "assert_eq!(",
        "assert!(",
        "assert!(matches!(",
        "assert_ne!(",
        "debug_assert!(",
    )

    def defect(self, quoted, source=None):
        return uninformative_assertion(
            f"examples/agent-workbench/src/main.rs:12#{quoted}",
            quoted if source is None else source,
        )

    def test_rejects_every_operandless_assert_opener(self):
        for opener in self.OPENERS:
            self.assertIn("no operands", self.defect(opener) or "", opener)

    def test_rejects_an_anchor_quoting_only_the_start_of_its_line(self):
        # The dropped tail is the assertion's whole meaning.
        defect = self.defect(
            "assert!(saw_gap,",
            '        assert!(saw_gap, "trimmed cursor should emit replay_gap");',
        )
        self.assertIn("quotes part of its line", defect)

    def test_accepts_a_line_that_states_the_observation(self):
        self.assertIsNone(self.defect('assert_eq!(scope.session_id, "session-finance");'))

    def test_accepts_an_operand_line_from_inside_a_multi_line_assertion(self):
        # Re-anchoring lands here: the line that carries the observed outcome.
        self.assertIsNone(self.defect('}) if turn_id == "transient-turn"'))
        self.assertIsNone(
            self.defect('"cancelling a parent must stop work guarded by its child"')
        )

    def test_reads_through_indentation(self):
        self.assertIsNone(
            self.defect("assert!(ok);", "            assert!(ok);")
        )

    def test_says_nothing_about_an_anchor_whose_line_cannot_be_read(self):
        # Resolution failures are already reported as stale references.
        self.assertIsNone(self.defect("assert_eq!(left, right);", ""))


class PerfunctoryExerciseTests(unittest.TestCase):
    """FIG-1345: syntax-only reachability is not example exercise."""

    def test_rejects_import_only_evidence(self):
        self.assertIn(
            "import",
            perfunctory_exercise(
                "lash::ModelSpec",
                "struct",
                "use lash::{LashCore, LashSession, ModelSpec, TurnWorkDriver};",
            )
            or "",
        )

    def test_rejects_constructor_and_type_signature_only_evidence(self):
        for source in (
            "let owner = LeaseOwnerIdentity::opaque(incarnation);",
            "let request = SessionCreateRequest::child_session(parent);",
            "Message {",
            "pub fn session_owner(incarnation: &str) -> LeaseOwnerIdentity {",
        ):
            self.assertIn(
                "construct",
                perfunctory_exercise("lash::LeaseOwnerIdentity", "struct", source) or "",
                source,
            )

    def test_rejects_a_constructor_function_call(self):
        for symbol, source in (
            (
                "lash::ModelSpec::builder",
                'let model = ModelSpec::builder("anthropic/claude-sonnet-4.6");',
            ),
            ("lash::PromptLayer::with_template", "PromptLayer::with_template(template)"),
            ("lash::Foo::create", "let foo = Foo::create(value);"),
            ("lash::Foo::project", "let projected = Foo::project(value);"),
        ):
            self.assertIn(
                "construct",
                perfunctory_exercise(symbol, "function", source) or "",
                symbol,
            )

    def test_rejects_a_multiline_import_continuation(self):
        self.assertIn(
            "import",
            perfunctory_exercise(
                "lash::ModelSpec",
                "struct",
                "use lash::{\n    LashCore, LashSession, ModelSpec, TurnWorkDriver,",
            )
            or "",
        )

    def test_rejects_variant_pattern_evidence(self):
        for source in (
            "SessionObservationEventPayload::ProcessChanged { process_ids, .. } => {",
            "TurnEvent::Usage {",
            "if err.code == RuntimeErrorCode::StoreCommitFailed =>",
            "SessionStartPoint::Empty,",
        ):
            self.assertIn(
                "variant pattern",
                perfunctory_exercise(
                    "lash::RuntimeErrorCode"
                    if "RuntimeErrorCode" in source
                    else "lash::SessionStartPoint"
                    if "SessionStartPoint" in source
                    else "lash::SessionObservationEventPayload::ProcessChanged",
                    "enum"
                    if "RuntimeErrorCode" in source or "SessionStartPoint" in source
                    else "variant",
                    source,
                )
                or "",
                source,
            )

    def test_rejects_fields_only_destructured_from_a_variant(self):
        self.assertIn(
            "variant pattern",
            perfunctory_exercise(
                "lash::TurnEvent::Usage::usage",
                "field",
                "usage, cumulative, ..",
            )
            or "",
        )

    def test_accepts_an_outcome_observation(self):
        self.assertIsNone(
            perfunctory_exercise(
                "lash::AttachmentCreateMeta",
                "struct",
                'assert_eq!(uploaded_ref.media_type().as_str(), "image/png");',
            )
        )

    def test_accepts_a_direct_variant_outcome_assertion(self):
        self.assertIsNone(
            perfunctory_exercise(
                "lash::CommitBudgetLimit::Bounded",
                "variant",
                "assert_eq!(budget.bytes, CommitBudgetLimit::Bounded(expected_bytes));",
            )
        )

    def test_does_not_let_a_separate_assertion_rescue_a_constructor_anchor(self):
        self.assertIn(
            "construct",
            perfunctory_exercise(
                "lash::CommitBudget",
                "struct",
                "let budget = CommitBudget::new(",
            )
            or "",
        )

    def test_accepts_a_syntax_only_unasserted_disposition(self):
        self.assertIsNone(
            perfunctory_exercise(
                "lash::persistence::AttachmentStore",
                "trait",
                "use lash::persistence::AttachmentStore;",
                "used-unasserted",
            )
        )


class UnrelatedFluentAssertionTests(unittest.TestCase):
    """FIG-1345: a setup call cannot inherit a callback's assertion."""

    def test_rejects_a_fluent_call_inheriting_a_match_guard(self):
        defect = unrelated_fluent_assertion(
            ".model(model)",
            '}) if turn_id == "transient-turn"',
        )
        self.assertIn("match guard", defect or "")

    def test_rejects_a_fluent_call_inheriting_a_closure_operand(self):
        defect = unrelated_fluent_assertion(
            ".admin()",
            ".map(|message| (message.id.as_str(), message.text.as_str()))",
        )
        self.assertIn("closure", defect or "")

    def test_accepts_a_fluent_call_with_a_direct_outcome_assertion(self):
        self.assertIsNone(
            unrelated_fluent_assertion(
                ".with_max_rows(8)",
                "assert_eq!(batching.max_rows(), 8);",
            )
        )

    def test_does_not_accept_free_form_prose_as_a_callback_exemption(self):
        defect = unrelated_fluent_assertion(
            ".active_manifests()",
            "assert!(!tool_names.iter().any(|name| name == removed));",
        )
        self.assertIn("closure", defect or "")

    def test_requires_an_explicit_relationship_value(self):
        defect = unrelated_fluent_assertion(
            ".model(model)",
            '}) if turn_id == "transient-turn"',
        )
        self.assertIn("match guard", defect or "")


class StaleDispositionReasonTests(unittest.TestCase):
    """A reason that describes another disposition is false, not untidy.

    Both wordings below shipped on rows holding the opposite evidence: the
    `unused-add` instruction on 904 rows that record real usage and a real
    assertion, and the `used-unasserted` wording on 121 rows that name an
    assertion anchor. Each is legal on the row it actually describes, so the lint
    has to read the disposition, not the prose alone.
    """

    #: The instruction 904 used-* rows carried, verbatim.
    ADD = (
        "Add lash::process::CausalRef::TriggerOccurrence to the agent-workbench "
        "durable-process example and assert its externally observable result."
    )
    #: The same instruction in the wording four more rows used: "to a ... example".
    ADD_WIDER = (
        "Add lash::process::SessionScope::for_agent_frame session provenance to a "
        "durable process example."
    )
    #: The used-unasserted wording, verbatim.
    UNASSERTED = (
        "The current example use of lash::process::ProcessProvenance is compile-only, "
        "setup-only, or reaches no executed assertion that independently observes its "
        "externally meaningful outcome; add an outcome-driven example test before "
        "claiming asserted usage."
    )
    #: What FIG-970 replaced them with: the row's own anchors, in words.
    DERIVED = (
        "Exercised by the agent-workbench example at "
        "examples/agent-workbench/src/main_sections/tests/process_work.rs:382; the "
        "assertion at examples/agent-workbench/src/main_sections/tests/"
        "process_work.rs:392 in `durable_process_registry_preserves_identity` observes "
        'that `session_id` equals `"session-finance"`.'
    )

    def test_rejects_the_add_instruction_on_a_row_that_records_usage(self):
        for disposition in ("used-asserted", "used-unasserted"):
            defect = stale_disposition_reason(disposition, self.ADD)
            self.assertIn("unused-add instruction", defect or "", disposition)

    def test_rejects_the_wider_add_wording_the_inventory_also_shipped(self):
        self.assertIsNotNone(stale_disposition_reason("used-asserted", self.ADD_WIDER))

    def test_leaves_the_add_instruction_legal_where_it_is_the_verdict(self):
        self.assertIsNone(stale_disposition_reason("unused-add", self.ADD))

    def test_rejects_denying_an_assertion_the_row_records(self):
        defect = stale_disposition_reason("used-asserted", self.UNASSERTED)
        self.assertIn("no executed assertion", defect or "")

    def test_leaves_that_wording_legal_on_an_unasserted_row(self):
        self.assertIsNone(stale_disposition_reason("used-unasserted", self.UNASSERTED))

    def test_accepts_a_reason_derived_from_the_rows_own_anchors(self):
        self.assertIsNone(stale_disposition_reason("used-asserted", self.DERIVED))

    def test_ignores_rows_that_record_no_reason(self):
        self.assertIsNone(stale_disposition_reason("used-asserted", ""))
        self.assertIsNone(stale_disposition_reason("used-asserted", "   "))


class MachineLocalPathTests(unittest.TestCase):
    #: The exact reason FIG-955 found in the inventory, absolute path included.
    FIGMENTS = (
        "Concrete consumer: downstream Figments app at "
        "/workspace/code/figments/apps/lash-runtime/src/main.rs:122 imports or uses "
        "lash_core::InputItem directly; retain this low-level alias until that caller "
        "migrates to the lash facade."
    )

    def test_rejects_the_absolute_paths_the_inventory_shipped_with(self):
        self.assertEqual(
            machine_local_path(self.FIGMENTS),
            "/workspace/code/figments/apps/lash-runtime/src/main.rs:122",
        )

    def test_rejects_home_relative_and_windows_roots(self):
        self.assertEqual(machine_local_path("see ~/checkouts/lash/src/lib.rs:4"), "~/checkouts/lash/src/lib.rs:4")
        self.assertEqual(machine_local_path(r"see C:\lash\src\lib.rs"), r"C:\lash\src\lib.rs")

    def test_accepts_repository_relative_evidence_and_ordinary_prose(self):
        self.assertIsNone(
            machine_local_path(
                "Internal integrator seam: consumed at crates/lash-core/src/model.rs:295."
            )
        )
        self.assertIsNone(machine_local_path("either/or, N/A, and a bare / separator"))
        self.assertIsNone(machine_local_path(""))

    def test_accepts_dot_slash_relative_evidence(self):
        # `./x/y` is as verifiable as `x/y`; stripping the leading dot used to
        # turn it into an absolute path and reject it.
        self.assertIsNone(machine_local_path("consumed at ./examples/foo/src/main.rs:12."))
        self.assertIsNone(machine_local_path("see ../sibling/src/lib.rs:3"))

    def test_rejects_a_unc_share(self):
        self.assertEqual(
            machine_local_path(r"see \\build-01\share\lash\src\lib.rs"),
            r"\\build-01\share\lash\src\lib.rs",
        )


class RepositoryPathExistenceTests(unittest.TestCase):
    def test_rejects_a_missing_repository_file_without_validating_its_line(self):
        citation = "crates/lash-plugin-plan-mode/src/lib.rs:999999"
        self.assertEqual(
            missing_repository_path(f"Consumer evidence: removed at {citation}."),
            citation,
        )

    def test_accepts_existing_files_with_or_without_line_anchors(self):
        self.assertIsNone(
            missing_repository_path(
                "Checked by ./scripts/check_api_example_coverage.py:999999 and "
                "recorded in docs/api-example-coverage.toml."
            )
        )

    def test_ignores_prose_that_does_not_cite_a_repository_file(self):
        self.assertIsNone(missing_repository_path("Keep this host-facing contract."))


class FacadeMigrationTests(unittest.TestCase):
    """The rule is the dependency cycle, not the sentence that describes it.

    A phrase regex cannot tell a promise from its negation, and the honest
    description of this defect *is* the negation: "that caller cannot migrate to
    the `lash` facade". Each case below pins a discrimination the regex alone got
    wrong -- broader wordings that must still be caught, the negation that must
    not be, and blame that must land on the crate the claim is about.
    """

    #: The exact reason FIG-955 found on forty `lash_core` entries.
    CORE = (
        "Concrete consumer: Lash workspace crate at "
        "crates/lash-core/src/runtime/effect/envelope.rs:435 imports or uses "
        "lash_core::AwaitEventKey directly; retain this low-level alias until that "
        "caller migrates to the lash facade."
    )
    FACADE_DIRS = {"crates/lash-core", "crates/lash-protocol-rlm"}

    def migration(self, reason):
        return impossible_facade_migration(reason, self.FACADE_DIRS)

    def test_rejects_a_migration_the_dependency_graph_forbids(self):
        self.assertEqual(
            self.migration(self.CORE),
            "crates/lash-core/src/runtime/effect/envelope.rs:435",
        )

    def test_rejects_the_same_promise_in_other_words(self):
        for wording in (
            "until that caller moves to the lash facade.",
            "until that caller migrates to the facade.",
            "pending a switch to the `lash` facade.",
            "we will port this caller to lash.",
        ):
            reason = self.CORE.rsplit("until that caller", 1)[0] + wording
            self.assertIsNotNone(self.migration(reason), wording)

    def test_allows_the_claim_for_a_crate_the_facade_is_not_built_on(self):
        reason = self.CORE.replace("crates/lash-core", "crates/lash-restate")
        self.assertIsNone(self.migration(reason))

    def test_allows_stating_that_the_caller_cannot_migrate(self):
        # Same sentence, same crate, same words -- opposite claim. The negation
        # is the correct description of the cycle, so it must pass.
        for denial in (
            "so that caller cannot migrate to the `lash` facade",
            "so that caller can never move to the lash facade",
            "that caller is unable to migrate to the lash facade",
        ):
            reason = (
                "Internal integrator seam: `lash_core::AwaitEventKey` is consumed at "
                f"crates/lash-core/src/runtime/effect/envelope.rs:435, and {denial}."
            )
            self.assertIsNone(self.migration(reason), denial)

    def test_blames_the_crate_the_claim_is_about_not_any_path_mentioned(self):
        reason = (
            "Concrete consumer: crates/lash-restate/src/lib.rs:588 uses this until that "
            "caller migrates to the lash facade. Defined at "
            "crates/lash-core/src/runtime/effect/envelope.rs:435."
        )
        self.assertIsNone(self.migration(reason))


class TestRegionTests(unittest.TestCase):
    """Where an example's host code stops and its tests begin."""

    SOURCE = [
        "fn host() {}",              # 1
        "#[cfg(test)]",              # 2
        "mod tests {",               # 3
        "    fn probe() {",          # 4
        "    }",                     # 5
        "}",                         # 6
        "fn more_host() {}",         # 7
        "#[cfg(all(test, feature = \"testing\"))]",  # 8
        "mod fixtures {",            # 9
        "}",                         # 10
    ]

    def test_spans_a_test_module_by_brace_depth(self):
        self.assertEqual(test_regions(self.SOURCE), [(2, 6), (8, 10)])

    def test_leaves_host_code_outside_every_region(self):
        regions = test_regions(self.SOURCE)
        self.assertFalse(any(start <= 1 <= end for start, end in regions))
        self.assertFalse(any(start <= 7 <= end for start, end in regions))

    def test_releases_a_gate_at_the_semicolon_of_a_bodyless_item(self):
        # FIG-1533: `#[cfg(test)] mod support;` is satisfied by the declaration
        # it sits on. Holding the gate open until the next brace handed it to
        # an unrelated shipped module, which then read as test code.
        source = [
            "#[cfg(test)]",            # 1
            "mod support;",            # 2
            "",                        # 3
            "pub mod shipped {",       # 4
            "    pub fn api() {}",     # 5
            "}",                       # 6
        ]
        # The declaration itself stays gated; the shipped module below does not.
        self.assertEqual(test_regions(source), [(1, 2)])

    def test_releases_a_gate_written_on_the_declaration_line(self):
        source = [
            "#[cfg(test)] mod support;",  # 1
            "pub mod shipped {",          # 2
            "}",                          # 3
        ]
        self.assertEqual(test_regions(source), [(1, 1)])

    def test_keeps_the_gated_statement_a_semicolon_ends(self):
        # FIG-1533 round 2: releasing the gate at the `;` must close a region
        # over the statement, not drop it. A `#[cfg(test)]` call spanning three
        # lines is test code on all three, and reading it as shipped is how a
        # test-only hook would pass for a crate's src/.
        source = [
            "impl Store {",                              # 1
            "    fn claim(&self) {",                     # 2
            "        #[cfg(test)]",                      # 3
            "        self.run_claim_after_lease_hook(",  # 4
            "            self.session_id(),",            # 5
            "        );",                                # 6
            "        self.commit();",                    # 7
            "    }",                                     # 8
            "}",                                         # 9
        ]
        regions = test_regions(source)
        self.assertEqual(regions, [(3, 6)])
        self.assertFalse(any(start <= 7 <= end for start, end in regions))

    def test_keeps_a_gated_use_statement_out_of_shipped_code(self):
        source = [
            "#[cfg(test)]",                       # 1
            "use super::InlineEffectHost;",       # 2
            "pub fn shipped() {}",                # 3
        ]
        self.assertEqual(test_regions(source), [(1, 2)])

    def test_does_not_release_a_gate_on_a_semicolon_inside_a_doc_comment(self):
        # Between the gate and its item sit doc comments and attributes; a
        # semicolon in either is prose or metadata, not the end of the item.
        source = [
            "#[cfg(test)]",                                  # 1
            "/// Enabled under test; never in a release.",   # 2
            "#[allow(clippy::unwrap_used)]",                 # 3
            "mod tests {",                                   # 4
            "    fn probe() {}",                             # 5
            "}",                                             # 6
            "pub fn shipped() {}",                           # 7
        ]
        regions = test_regions(source)
        self.assertEqual(regions, [(1, 6)])
        self.assertFalse(any(start <= 7 <= end for start, end in regions))

    def test_does_not_open_a_body_on_a_brace_inside_a_literal(self):
        source = [
            "#[cfg(test)]",                          # 1
            'const PROBE: &str = "{ not a body";',   # 2
            "pub fn shipped() {",                    # 3
            "}",                                     # 4
        ]
        regions = test_regions(source)
        self.assertEqual(regions, [(1, 2)])
        self.assertFalse(any(start <= 3 <= end for start, end in regions))

    def test_still_sees_a_real_gate_after_a_bodyless_declaration(self):
        # The swallowed-gate half of the same defect: the leaked region ran to
        # the end of the file, so the `#[cfg(test)] mod tests { .. }` below it
        # never opened a region of its own.
        source = [
            "#[cfg(test)]",            # 1
            "mod support;",            # 2
            "pub mod shipped {",       # 3
            "    pub fn api() {}",     # 4
            "}",                       # 5
            "#[cfg(test)]",            # 6
            "mod tests {",             # 7
            "    fn probe() {}",       # 8
            "}",                       # 9
        ]
        self.assertEqual(test_regions(source), [(1, 2), (6, 9)])


class OutOfLineTestModuleTests(unittest.TestCase):
    """`#[cfg(test)] mod x;` puts the gate on the declaration, not on the file."""

    SOURCE = [
        "use crate::thing::Thing;",
        "",
        "#[cfg(test)]",
        "mod support;",
        "",
        "#[cfg(test)]",
        "#[allow(clippy::unwrap_used)]",
        "pub(crate) mod probes;",
        "",
        "mod shipped;",
        "",
        "#[cfg(feature = \"testing\")]",
        "mod fixtures;",
        "",
        "#[cfg(test)]",
        "mod inline {",
        "    mod nested;",
        "}",
    ]

    def test_reads_every_cfg_predicate_that_gates_on_tests(self):
        self.assertTrue(cfg_gates_test("#[cfg(test)]"))
        self.assertTrue(cfg_gates_test('#[cfg(all(test, feature = "testing"))]'))
        self.assertTrue(cfg_gates_test('#[cfg(all(test, any(unix, windows)))]'))
        # A prefix match read `all(test, ...)` as shipped code, which is how a
        # provider's conformance route counted as another crate's src/.
        self.assertFalse(cfg_gates_test("#[cfg(not(test))]"))
        self.assertFalse(cfg_gates_test("#[cfg(all(not(test), unix))]"))
        self.assertFalse(cfg_gates_test('#[cfg(feature = "testing")]'))
        self.assertFalse(cfg_gates_test("fn test_helper() {}"))

    def test_inverts_a_negated_predicate_through_the_evaluator(self):
        # `not` is the one operator that changes the answer of everything under
        # it, so it is asserted against the evaluator directly rather than only
        # through the gate spellings above.
        self.assertTrue(evaluate_cfg(parse_cfg("not(test)"), set()))
        self.assertFalse(evaluate_cfg(parse_cfg("not(test)"), {"test"}))
        self.assertTrue(
            evaluate_cfg(parse_cfg('not(any(test, feature = "sim"))'), {"unix"})
        )
        self.assertFalse(
            evaluate_cfg(parse_cfg('not(any(test, feature = "sim"))'), {"test"})
        )
        # A `cfg` that only ever compiles without tests is shipped code, and one
        # that demands tests *and* the absence of a feature is not.
        self.assertFalse(cfg_gates_test('#[cfg(not(feature = "sim"))]'))
        self.assertTrue(cfg_gates_test('#[cfg(all(test, not(feature = "sim")))]'))
        # Rust's `not` takes one predicate; a malformed `not(a, b)` is refused
        # rather than read as "not both".
        with self.assertRaises(AssertionError):
            evaluate_cfg(parse_cfg("not(test, unix)"), set())

    def test_reads_an_any_gate_as_the_shipped_code_it_compiles_to(self):
        # FIG-1533: `any(test, ...)` ships whenever the other arm is on, so the
        # item is shipped code that tests also happen to see. Reading the bare
        # mention of `test` as a test gate filed a whole directory of shipped
        # feature-gated conversions as tests.
        self.assertFalse(cfg_gates_test('#[cfg(any(test, feature = "sim"))]'))
        self.assertFalse(
            cfg_gates_test('#[cfg(any(feature = "core-conversions", test))]')
        )
        self.assertFalse(cfg_gates_test('#[cfg(all(any(test, feature = "sim")))]'))
        self.assertFalse(
            cfg_gates_test('#[cfg(any(all(test, unix), feature = "sim"))]')
        )
        # Nesting an `any` under an `all` that also demands `test` still never
        # reaches a shipped build.
        self.assertTrue(
            cfg_gates_test('#[cfg(all(test, any(feature = "sim", unix)))]')
        )

    def test_reads_a_de_facto_test_atom_as_the_free_cfg_flag_it_is(self):
        # Recorded, not incidental: `miri` is a test-runner flag by convention,
        # but the evaluator knows only `test`. `any(test, miri)` therefore reads
        # as shipped -- a miri build compiles it without cfg(test) -- and the
        # tier follows the compiler rather than the convention. Change this
        # assertion, not the classifier, if the workspace ever wants otherwise.
        self.assertFalse(cfg_gates_test("#[cfg(all(any(test, miri), unix))]"))
        self.assertFalse(cfg_gates_test("#[cfg(miri)]"))

    def test_reads_one_feature_however_its_spacing_is_written(self):
        for predicate in (
            '#[cfg(any(test, feature = "sim"))]',
            '#[cfg(any(test,feature="sim"))]',
            '#[cfg(any(test,  feature   =  "sim"))]',
        ):
            self.assertFalse(cfg_gates_test(predicate), predicate)
        self.assertEqual(
            parse_cfg('feature="sim"'), parse_cfg('feature = "sim"')
        )

    def test_keeps_the_feature_gated_test_homes_out_of_the_shipped_tier(self):
        # FIG-1533 ruling: the `testing` / `test_support` modules are where a
        # `Relocate:` note sends a test-only item, so their files answer to the
        # test tiers even though a downstream build can turn the feature on.
        # Reading them as crate-src would let an internal seam prove itself by
        # citing the test harness -- the amnesty FIG-1223 closed.
        for path in (
            "crates/lash-core/src/testing.rs",
            "crates/lash-core/src/testing/conformance/artifact_store.rs",
            "crates/lash-core/src/test_support.rs",
            "crates/lash-core/src/store/testing.rs",
            "crates/lash-core/src/runtime/process/testing/continuation.rs",
            "crates/lash-core/src/runtime/in_memory_store/testing_access.rs",
            "crates/lash-provider-openai/src/codex/ws_testing.rs",
            "crates/lash/src/testing.rs",
            "crates/lashlang/src/testing.rs",
        ):
            self.assertTrue(feature_gated_test_home(path), path)
            self.assertEqual(
                anchor_tier(f"{path}:1#let _ = x;"), "workspace-tests", path
            )
        # A feature-gated module that is not a test home stays shipped code.
        for path in (
            "crates/lash-remote-protocol/src/core_conversions.rs",
            "crates/lash-core/src/runtime/turn_loop.rs",
        ):
            self.assertFalse(feature_gated_test_home(path), path)

    def test_tiers_an_any_test_module_as_shipped_source(self):
        # `crates/lash-remote-protocol/src/lib.rs` declares `core_conversions`
        # under `any(feature = "core-conversions", test)`: the module ships with
        # that feature on, so neither it nor its directory is test code.
        self.assertFalse(
            test_path("crates/lash-remote-protocol/src/core_conversions.rs")
        )
        self.assertEqual(
            anchor_tier(
                "crates/lash-remote-protocol/src/core_conversions.rs:40#let _ = x;"
            ),
            "crate-src",
        )

    def test_reads_the_modules_a_test_gate_declares(self):
        self.assertEqual(
            declared_test_modules(self.SOURCE), [("support", None), ("probes", None)]
        )

    def test_leaves_shipped_and_feature_gated_declarations_alone(self):
        declared = [name for name, _ in declared_test_modules(self.SOURCE)]
        self.assertNotIn("shipped", declared)
        self.assertNotIn("fixtures", declared)

    def test_reads_a_gate_and_declaration_written_on_one_line(self):
        self.assertEqual(
            declared_test_modules(["#[cfg(test)] mod inline_probe;"]),
            [("inline_probe", None)],
        )

    def test_reads_a_path_attribute_as_the_modules_real_location(self):
        self.assertEqual(
            declared_test_modules(
                [
                    "#[cfg(test)]",
                    '#[path = "core_conversions_tests.rs"]',
                    "mod core_conversions_tests;",
                ]
            ),
            [("core_conversions_tests", "core_conversions_tests.rs")],
        )

    def test_forgets_a_path_attribute_that_belonged_to_shipped_code(self):
        declared = declared_test_modules(
            ['#[path = "generated/shipped.rs"]', "mod shipped;", "#[cfg(test)]", "mod probes;"]
        )
        self.assertEqual(declared, [("probes", None)])

    def test_tiers_the_three_files_that_read_as_shipped_source(self):
        # Each is test code only by its declaration, and each was `crate-src`
        # before this rule: an `all(test, ...)` gate and two `#[path]` modules.
        for path in (
            "crates/lash-provider-google/src/conformance_route.rs",
            "crates/lash-remote-protocol/src/core_conversions_tests.rs",
            "crates/lash-postgres-store/src/postgres/checkpoint_depth_tests.rs",
        ):
            self.assertTrue(test_path(path), path)
            self.assertEqual(anchor_tier(f"{path}:12#let _ = x;"), "workspace-tests", path)

    def test_resolves_a_declaration_to_the_directory_it_owns(self):
        self.assertEqual(module_directory("crates/c/src/a/b.rs"), "crates/c/src/a/b")
        self.assertEqual(module_directory("crates/c/src/a/mod.rs"), "crates/c/src/a")
        self.assertEqual(module_directory("crates/c/src/lib.rs"), "crates/c/src")

    def test_finds_repository_files_that_are_test_code_only_by_declaration(self):
        files, directories = test_module_paths()
        self.assertTrue(files, "no #[cfg(test)] mod declarations found at all")
        positional = {"test", "tests", "bench", "benches"}
        undeclared = [
            path
            for path in files
            if not set(path.split("/")) & {"tests", "benches"}
            and path.split("/")[-1].removesuffix(".rs") not in positional
        ]
        self.assertTrue(
            undeclared,
            "every declared test module was already test code by its path, so "
            "this rule would be untested",
        )
        for path in undeclared:
            self.assertTrue(test_path(path), path)
            self.assertIn(
                anchor_tier(f"{path}:1#x"), {"workspace-tests", "example-test"}, path
            )

    def test_carries_a_test_modules_own_submodules_with_it(self):
        _, directories = test_module_paths()
        self.assertTrue(all(directory.endswith("/") for directory in directories))
        self.assertTrue(test_path(f"{next(iter(directories))}deeper/inner.rs"))


class MemberAnchorTests(unittest.TestCase):
    """A member's leaf name is not evidence; the owning type is.

    `SchemaDialect::as_str` was "proved" by `serde_json`'s `value.as_str()`, two
    unrelated members by the same `row.get(3)`, and a variant's field by a
    `tungstenite` import alias -- all under a substring match (FIG-1223).
    """

    FILE = "crates/lash-fixture/src/consumer.rs"
    SOURCE = [
        "use serde_json::Value;",                                    # 1
        "use lash_core::facade_support::SchemaDialect;",              # 2
        "",                                                          # 3
        "fn coincidence(value: &Value) -> Option<&str> {",            # 4
        "    value.as_str()",                                        # 5
        "}",                                                         # 6
        "",                                                          # 7
        "fn honest(dialect: SchemaDialect) -> &'static str {",        # 8
        "    dialect.as_str()",                                      # 9
        "}",                                                         # 10
        "",                                                          # 11
        "impl SchemaDialect for Local {",                            # 12
        "    fn as_str(&self) -> &'static str {",                    # 13
        "        \"local\"",                                         # 14
        "    }",                                                     # 15
        "}",                                                         # 16
        "",                                                          # 17
        "fn crowded(a: SchemaDialect, b: WireDialect) -> usize {",    # 18
        "    a.as_str().len() + b.as_str().len()",                   # 19
        "}",                                                         # 20
        "",                                                          # 21
        "fn qualified() -> &'static str {",                          # 22
        "    SchemaDialect::as_str(&SchemaDialect::Json)",           # 23
        "}",                                                         # 24
        "",                                                          # 25
        "fn unbound(input: &Value, dialect: Dialectish) -> usize {",  # 26
        "    let _ = SchemaDialect::Json;",                          # 27
        "    input.as_str().map(str::len).unwrap_or(0)",              # 28
        "}",                                                         # 29
    ]

    SYMBOL = "lash::schema::SchemaDialect::as_str"

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        _RESOLVED_RECEIVERS.clear()
        _IMPORTED_TYPES.pop(self.FILE, None)
        _LITERAL_STACKS.pop(self.FILE, None)
        self.addCleanup(_LITERAL_STACKS.pop, self.FILE, None)
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)
        self.addCleanup(_IMPORTED_TYPES.pop, self.FILE, None)

    def anchor(self, line):
        return f"{self.FILE}:{line}#{self.SOURCE[line - 1].strip()}"

    def defect(self, line, symbol=None, rivals=frozenset()):
        return member_anchor_defect(
            symbol or self.SYMBOL, [], "function", set(rivals), self.anchor(line)
        )

    def test_accepts_a_line_that_qualifies_the_member_by_its_owner(self):
        self.assertIsNone(self.defect(23, rivals={"WireDialect"}))

    def test_rejects_an_import_of_the_owning_type_as_member_evidence(self):
        # Importing the type says nothing about the member; that is the whole
        # gap a substring match could not see.
        self.assertIsNotNone(self.defect(2))

    def test_accepts_a_receiver_whose_type_the_function_establishes(self):
        self.assertIsNone(self.defect(9))

    def test_rejects_an_unrelated_same_leaf_call(self):
        defect = self.defect(5)
        self.assertIsNotNone(defect)
        self.assertIn("rather than SchemaDialect", defect)

    def test_accepts_a_crowded_line_when_the_receiver_is_bound(self):
        # Two same-leaf receivers on one line, each bound by the signature: the
        # binding is what disambiguates them.
        self.assertIsNone(self.defect(19, rivals={"WireDialect"}))

    def test_rejects_an_unbound_receiver_with_a_rival_in_scope(self):
        defect = self.defect(28, rivals={"WireDialect"})
        self.assertIsNotNone(defect)
        self.assertIn("reaches the member through `input`", defect)

    def test_reads_a_trait_impl_header_as_part_of_the_scope(self):
        self.assertIsNone(self.defect(13))

    def test_rejects_an_anchor_whose_file_cannot_be_read(self):
        defect = member_anchor_defect(
            self.SYMBOL, [], "function", set(), "crates/gone/src/lib.rs:4#x.as_str()"
        )
        self.assertIn("does not resolve", defect or "")

    def test_holds_no_opinion_about_a_type_level_item(self):
        self.assertIsNone(
            member_anchor_defect("lash::schema::SchemaDialect", [], "struct", set(), self.anchor(5))
        )

    def test_requires_the_type_a_nested_members_owner_belongs_to(self):
        # A variant name is as generic as a member name, so `Message::0` was
        # "proved" by an import aliasing tungstenite's `Message`.
        nested = "lash::schema::SchemaDialect::Local::as_str"
        defect = self.defect(5, symbol=nested)
        self.assertIn("rather than Local", defect or "")
        self.assertIsNone(self.defect(13, symbol=nested))

    def test_names_a_nested_members_containing_type_but_never_a_module(self):
        self.assertEqual(
            member_containers(
                "lash_core::facade_support::BorrowedChronologicalPayload::Message::0",
                [],
                "field",
            ),
            {"BorrowedChronologicalPayload"},
        )
        self.assertEqual(
            member_containers("lash::persistence::RuntimeCommit::budget", [], "function"),
            set(),
        )

    def test_reads_the_receivers_binding_before_trusting_a_rival(self):
        scope = "\n".join(self.SOURCE[7:10])
        self.assertTrue(binds_receiver(scope, "dialect", {"SchemaDialect"}))
        self.assertFalse(
            binds_receiver("\n".join(self.SOURCE[3:6]), "value", {"SchemaDialect"})
        )

    def test_counts_an_imported_external_type_as_a_rival(self):
        # The motivating coincidence is not Lash API at all: serde_json's
        # `Value::as_str` satisfied a substring match for `SchemaDialect::as_str`.
        defect = self.defect(19, symbol="lash::schema::Local::as_str")
        self.assertIsNotNone(defect)

    def test_applies_the_rival_check_even_when_the_line_mentions_the_owner(self):
        # Mentioning is not qualifying: this line names `Text` and settles nothing.
        self.assertTrue(qualifies_member("Text", "text", "RemoteInputItem::Text { text: t }"))
        self.assertFalse(qualifies_member("Text", "text", "let text = Text::render();"))

    def test_scopes_a_line_to_its_innermost_function(self):
        blocks = scope_blocks(self.SOURCE)
        self.assertIn((3, 5, "fn"), blocks)
        self.assertIn((11, 15, "impl"), blocks)
        scope = anchor_scope(self.FILE, 5)
        self.assertIn("value.as_str()", scope)
        self.assertNotIn("dialect.as_str()", scope)

    def test_owners_come_from_every_path_the_item_has(self):
        self.assertEqual(
            member_owners(
                "lash::Turn::id", ["lash_core::facade_support::TurnRecord::id"], "field"
            ),
            {"Turn", "TurnRecord"},
        )
        self.assertEqual(member_owners("lash::Turn", [], "struct"), set())

    def test_collects_the_rivals_a_leaf_has_across_the_ledger(self):
        owners = member_leaf_owners(
            [
                {"symbol": "lash::Turn::id", "kind": "field"},
                {"symbol": "lash::Session::id", "kind": "field", "aliases": ["lash_core::Sess::id"]},
                {"symbol": "lash::Widget", "kind": "struct"},
            ]
        )
        self.assertEqual(owners["id"], {"Turn", "Session", "Sess"})
        self.assertNotIn("Widget", owners)


class ReceiverResolutionTests(unittest.TestCase):
    """A receiver has to resolve to the owning type, named rival or not.

    The rival check could only reject what it could name, so a receiver typed by
    the prelude, by a file-local type, or by a path-qualified type nobody
    imported passed unchallenged -- and 11 member anchors rested on that branch.
    A receiver the reader cannot follow to the owning type is a defect on its
    own (FIG-1223).
    """

    FILE = "crates/lash-fixture/src/receiver.rs"
    SOURCE = [
        "type PostgresEffectReplay = EffectReplayDriver<Persistence, Backend>;",  # 1
        "",                                                                      # 2
        "struct TurnInput {",                                                    # 3
        "    turn_context: TurnContext,",                                        # 4
        "}",                                                                     # 5
        "",                                                                      # 6
        "struct Turn {",                                                         # 7
        "    input: TurnInput,",                                                  # 8
        "}",                                                                     # 9
        "",                                                                      # 10
        "impl TurnContextFacadeOps for TurnContext {",                            # 11
        "    fn clear_prompt_slot(&self, slot: Slot) {}",                        # 12
        "}",                                                                     # 13
        "",                                                                      # 14
        "impl Turn {",                                                            # 15
        "    fn drop_slot(&self, slot: Slot) {",                                  # 16
        "        self.input.turn_context.clear_prompt_slot(slot);",               # 17
        "    }",                                                                  # 18
        "}",                                                                      # 19
        "",                                                                      # 20
        "struct Store {",                                                         # 21
        "    inner: PostgresEffectReplay,",                                       # 22
        "}",                                                                      # 23
        "",                                                                      # 24
        "impl Store {",                                                           # 25
        "    async fn retire(",                                                    # 26
        "        &self,",                                                          # 27
        "        retirement: Retirement,",                                          # 28
        "    ) -> Result<(), StoreError> {",                                        # 29
        "        self.inner.retire_effect_journal(retirement).await",                # 30
        "    }",                                                                    # 31
        "",                                                                        # 32
        "    fn unrelated(&self, slot: Slot) {",                                    # 33
        "        let text = String::new();",                                        # 34
        "        text.clear_prompt_slot(slot);",                                    # 35
        "    }",                                                                    # 36
        "}",                                                                        # 37
        "",                                                                        # 38
        "fn build(call: SourceCall) -> CompletedToolCall {",                        # 39
        "    CompletedToolCall {",                                                  # 40
        "        call_id: call.call_id.clone(),",                                    # 41
        "        model_return: ModelToolReturn {",                                   # 42
        "            call_id: call.call_id.clone(),",                                # 43
        "        },",                                                               # 44
        "    }",                                                                    # 45
        "}",                                                                        # 46
    ]

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        _RESOLVED_RECEIVERS.clear()
        _IMPORTED_TYPES.pop(self.FILE, None)
        # The type index is built once per run over every readable source file,
        # so a test that supplies one has to let it be rebuilt.
        _TYPE_FACTS.clear()
        _LITERAL_STACKS.pop(self.FILE, None)
        self.addCleanup(_TYPE_FACTS.clear)
        self.addCleanup(_LITERAL_STACKS.pop, self.FILE, None)
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)
        self.addCleanup(_IMPORTED_TYPES.pop, self.FILE, None)

    def anchor(self, line):
        return f"{self.FILE}:{line}#{self.SOURCE[line - 1].strip()}"

    def defect(self, symbol, line, kind="function", rivals=frozenset()):
        return member_anchor_defect(symbol, [], kind, set(rivals), self.anchor(line))

    def test_accepts_a_receiver_that_resolves_through_fields_to_the_owner(self):
        # `self.input.turn_context` is three hops from the `impl` block to a type
        # that implements the owning trait; the owning trait is never named on
        # the line or in the function.
        self.assertIsNone(
            self.defect("lash_core::facade_support::TurnContextFacadeOps::clear_prompt_slot", 17)
        )

    def test_follows_a_type_alias_to_the_type_that_owns_the_member(self):
        # `type PostgresEffectReplay = EffectReplayDriver<..>` is the only line
        # that ties the store's field to the driver whose method this is.
        self.assertIsNone(
            self.defect(
                "lash_core::facade_support::effect_replay_driver::"
                "EffectReplayDriver::retire_effect_journal",
                30,
            )
        )

    def test_rejects_a_prelude_typed_receiver_no_rival_check_could_name(self):
        # `String` is neither a ledger item nor an import, so the rival check saw
        # nothing to object to and passed the line.
        defect = self.defect(
            "lash_core::facade_support::TurnContextFacadeOps::clear_prompt_slot", 35
        )
        self.assertIsNotNone(defect)
        self.assertIn("rather than TurnContextFacadeOps", defect)

    def test_rejects_a_receiver_that_resolves_to_nothing_this_repo_declares(self):
        defect = self.defect("lash::store::Store::retire_effect_journal", 30)
        self.assertIsNotNone(defect)
        self.assertIn("rather than Store", defect)

    def test_reads_a_field_anchor_against_the_literal_it_sits_in(self):
        # Adjacent literals write `call_id` twice for two different types.
        symbol = "lash_core::facade_support::ModelToolReturn::call_id"
        defect = self.defect(symbol, 41, kind="field")
        self.assertIsNotNone(defect)
        self.assertIn("inside a CompletedToolCall literal", defect)
        self.assertIsNone(self.defect(symbol, 43, kind="field"))

    def test_keeps_functions_apart_when_a_signature_spans_lines(self):
        # `) -> Result<(), StoreError> {` names no function, and a function
        # nobody can find is a scope the size of the file: two `retirement`
        # bindings from two functions then answer for each other.
        scope = anchor_scope(self.FILE, 30)
        self.assertIn("self.inner.retire_effect_journal", scope)
        self.assertNotIn("let text = String::new();", scope)


class MultiLineReceiverTests(unittest.TestCase):
    """A fluent chain is written down the page; its receiver is not on the line.

    Reading only the anchor's own line found no receiver at all, and the
    fail-closed rule then turned 26 live consumers into deletion instructions
    (FIG-1223).
    """

    FILE = "crates/lash-fixture/src/fluent.rs"
    SOURCE = [
        "struct Env {",                                                  # 1
        "    control: ControlConfig,",                                   # 2
        "}",                                                             # 3
        "",                                                              # 4
        "struct ControlConfig {",                                        # 5
        "    effect_host: Arc<dyn EffectHost>,",                         # 6
        "}",                                                             # 7
        "",                                                              # 8
        "enum StreamEvent {",                                            # 9
        "    Part(OutputPart),",                                         # 10
        "    Delta(String),",                                            # 11
        "}",                                                             # 12
        "",                                                              # 13
        "impl Env {",                                                    # 14
        "    fn retire(&self, session: SessionId) -> Result<()> {",      # 15
        "        self.control",                                          # 16
        "            .effect_host",                                      # 17
        "            .retire_effect_journal(Retirement::session(&session))", # 18
        "    }",                                                         # 19
        "",                                                              # 20
        "    fn scope(&self, host: &dyn EffectHost) -> Result<()> {",    # 21
        "        let scoped: Scoped = host",                             # 22
        "            .scoped_static(self.turn_scope())?",                # 23
        "            .ok_or(Error::Static)?;",                           # 24
        "        let outcome = scoped",                                   # 25
        "            .controller()",                                     # 26
        "            .execute_effect(Envelope::new())?;",                # 27
        "        Ok(())",                                                # 28
        "    }",                                                         # 29
        "",                                                              # 30
        "    fn stamp(&self, mut event: StreamEvent, route: &Route) {",  # 31
        "        if let StreamEvent::Part(part) = &mut event {",         # 32
        "            let _ = part.stamp_replay_origin(route);",          # 33
        "        }",                                                     # 34
        "    }",                                                        # 35
        "}",                                                            # 36
        "",                                                             # 37
        "enum WireEvent {",                                             # 38
        "    RetryStatus {",                                            # 39
        "        attempt: usize,",                                       # 40
        "    },",                                                       # 41
        "}",                                                            # 42
        "",                                                             # 43
        "fn wire() -> WireEvent {",                                     # 44
        "    WireEvent::RetryStatus { attempt: 1 }",                     # 45
        "}",                                                            # 46
        "",                                                             # 47
        "struct Scoped {}",                                             # 48
        "",                                                             # 49
        "impl Scoped {",                                                # 50
        "    fn controller(&self) -> RuntimeEffectController {",         # 51
        "        self.inner.clone()",                                    # 52
        "    }",                                                        # 53
        "}",                                                            # 54
        "",                                                             # 55
        "struct Wrapper {",                                             # 56
        "    inner: lash_core::facade_support::Env,",                    # 57
        "}",                                                            # 58
        "",                                                             # 59
        "struct RemoteUsage {",                                         # 60
        "    output_tokens: u64,",                                       # 61
        "}",                                                            # 62
        "",                                                             # 63
        "impl From<TokenUsage> for RemoteUsage {}",                      # 64
        "",                                                             # 65
        "impl RemoteUsage {",                                           # 66
        "    fn add(&mut self, other: &Self) {",                         # 67
        "        self.output_tokens = other.output_tokens;",             # 68
        "    }",                                                        # 69
        "",                                                             # 70
        "    fn borrowed(&self, map: &Registry, key: &Key) {",           # 71
        "        if let Some(found) = map.get(key) {",                    # 72
        "            let _ = found.output_tokens;",                       # 73
        "        }",                                                     # 74
        "    }",                                                        # 75
        "}",                                                            # 76
        "",                                                             # 77
        "struct ToolState {}",                                          # 78
        "",                                                             # 79
        "impl ToolStateFacadeOps for ToolState {}",                      # 80
        "",                                                             # 81
        "struct Admin {}",                                              # 82
        "",                                                             # 83
        "impl Admin {",                                                 # 84
        "    async fn tool_state(&self) -> Result<ToolState> {",         # 85
        "        Ok(self.state.clone())",                                # 86
        "    }",                                                        # 87
        "",                                                             # 88
        "    async fn newest(&self) -> Result<u64> {",                   # 89
        "        Ok(self.tool_state().await?.generation())",             # 90
        "    }",                                                        # 91
        "",                                                             # 92
        "    fn manifests(&self) -> Result<Vec<Manifest>> {",            # 93
        "        Ok(self.tool_state()?.tool_manifests())",               # 94
        "    }",                                                        # 95
        "}",                                                            # 96
        "",                                                             # 97
        "enum TurnFinish {",                                            # 98
        "    FinalValue { value: Value },",                              # 99
        "}",                                                            # 100
        "",                                                             # 101
        "fn read(finish: &TurnFinish, event: &TurnEvent) {",             # 102
        "    if let TurnFinish::FinalValue { value } = finish {}",        # 103
        "    if let TurnEvent::FinalValue { value } = event {}",          # 104
        "}",                                                            # 105
        "",                                                             # 106
        "impl ToolStateFacadeOps for OtherState {",                      # 107
        "    fn generation(&self) -> u64 {",                             # 108
        "        0",                                                     # 109
        "    }",                                                        # 110
        "}",                                                            # 111
        "",                                                             # 112
        "mod harness {",                                                # 113
        "    use lash_core::{",                                         # 114
        "        TurnDriverPreamble,",                                   # 115
        "    };",                                                       # 116
        "",                                                             # 117
        "    fn build() -> AnthropicProvider {",                        # 118
        "        use lash_core::facade_support::ParkedSession;",         # 119
        "        let provider = AnthropicProvider::new(\"key\");",       # 120
        "        provider.with_parked(ParkedSession::new())",            # 121
        "    }",                                                       # 122
        "}",                                                           # 123
    ]

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        _RESOLVED_RECEIVERS.clear()
        _IMPORTED_TYPES.pop(self.FILE, None)
        _LITERAL_STACKS.pop(self.FILE, None)
        _TYPE_FACTS.clear()
        self.addCleanup(_TYPE_FACTS.clear)
        self.addCleanup(_LITERAL_STACKS.pop, self.FILE, None)
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)
        self.addCleanup(_IMPORTED_TYPES.pop, self.FILE, None)

    def anchor(self, line):
        return f"{self.FILE}:{line}#{self.SOURCE[line - 1].strip()}"

    def defect(self, symbol, line, kind="function"):
        return member_anchor_defect(symbol, [], kind, set(), self.anchor(line))

    def test_assembles_a_chain_split_across_lines(self):
        # `self.control` / `.effect_host` / `.retire_effect_journal(..)`: the
        # receiver is two lines above the anchor.
        self.assertIsNone(
            self.defect("lash::durability::EffectHost::retire_effect_journal", 18)
        )

    def test_assembles_a_chain_whose_receiver_is_a_signature_parameter(self):
        self.assertIsNone(self.defect("lash::durability::EffectHost::scoped_static", 23))

    def test_carries_a_method_return_type_across_a_split_chain(self):
        self.assertIsNone(
            self.defect("lash::runtime::RuntimeEffectController::execute_effect", 27)
        )

    def test_types_a_binding_a_variant_pattern_introduces(self):
        # `if let StreamEvent::Part(part)` is the only line that types `part`, and
        # round 4 read it as no type at all and filed the path for removal.
        self.assertIsNone(self.defect("lash::direct::OutputPart::stamp_replay_origin", 33))

    def test_rejects_a_field_declaration_in_another_crates_own_enum(self):
        symbol = "lash_core::facade_support::SessionStreamEvent::RetryStatus::attempt"
        defect = self.defect(symbol, 40, kind="field")
        self.assertIsNotNone(defect)
        self.assertIn("sits inside the declaration of WireEvent", defect)

    def test_requires_the_container_a_nested_field_belongs_to(self):
        # `WireEvent::RetryStatus { attempt: 1 }` writes another crate's variant,
        # so the literal resolves to the wrong container.
        symbol = "lash_core::facade_support::SessionStreamEvent::RetryStatus::attempt"
        defect = self.defect(symbol, 45, kind="field")
        self.assertIsNotNone(defect)
        self.assertIn("SessionStreamEvent", defect)

    def test_reads_only_the_traits_own_name_from_an_impl_header(self):
        # `impl From<TokenUsage> for RemoteUsage` implements `From`; reading its
        # argument as a trait made every RemoteUsage field evidence for
        # TokenUsage's field of the same name.
        defect = self.defect("lash::usage::TokenUsage::output_tokens", 68, kind="field")
        self.assertIsNotNone(defect)
        self.assertIn("resolves to RemoteUsage", defect)

    def test_refuses_to_type_a_binding_from_an_option_pattern(self):
        # `Some(found)` says nothing about the type, and guessing from every
        # `Some(..)` payload in the workspace gave one receiver forty types.
        defect = self.defect("lash::usage::TokenUsage::output_tokens", 73, kind="field")
        self.assertIsNotNone(defect)
        self.assertIn("no type this repository declares", defect)

    def test_rejects_an_anchor_in_the_file_that_declares_the_item(self):
        for anchor in (self.anchor(15), self.anchor(1)):
            self.assertIn(
                "declares",
                declaration_anchor_defect("lash_core::sansio::Env", anchor) or "",
            )

    def test_accepts_a_path_qualified_anchor_in_a_file_declaring_the_same_name(self):
        # `crates/lash/src/session.rs` declares its own `ParkedSession` wrapper
        # around `lash_core::facade_support::ParkedSession`; the path settles it.
        self.assertIsNone(
            declaration_anchor_defect("lash_core::facade_support::Env", self.anchor(57))
        )

    def test_peels_await_and_question_marks_out_of_a_chain(self):
        # `self.tool_state().await?.generation()`: the future and the `Result` are
        # unwraps, not hops, and reading them as unknown hops made every fallible
        # accessor unresolvable -- and its path deletable.
        symbol = "lash_core::facade_support::ToolStateFacadeOps::generation"
        self.assertIsNone(self.defect(symbol, 90))

    def test_peels_a_bare_question_mark_out_of_a_chain(self):
        symbol = "lash_core::facade_support::ToolStateFacadeOps::tool_manifests"
        self.assertIsNone(self.defect(symbol, 94))

    def test_rejects_qualification_by_a_different_container(self):
        # `TurnFinish::FinalValue { value }` and `TurnEvent::FinalValue { value }`
        # write the same variant name under two different enums.
        symbol = "lash::TurnEvent::FinalValue::value"
        defect = self.defect(symbol, 103, kind="field")
        self.assertIsNotNone(defect)
        self.assertIn("under TurnFinish, not under TurnEvent", defect)
        self.assertIsNone(self.defect(symbol, 104, kind="field"))

    def test_treats_an_import_as_reachability_not_consumption(self):
        # Ruled for FIG-1223: a `use` resolves whether or not anything needs the
        # item, so it cannot carry an internal seam's dependency claim.
        for line in (
            "crates/lash-sansio/src/lib.rs:85#pub use turn::{PreparedTurnMachine};",
            "crates/lash-sim/src/store.rs:4#use lash_core::facade_support::ParkedSession;",
        ):
            self.assertIn("is an import", import_anchor_defect(line) or "")

    def test_rejects_only_the_lines_a_use_list_actually_spans(self):
        # A brace-list continuation is still an import; the line *after* a
        # completed `use ...;` is not.  Testing the `use` before the statement
        # break read every line under an indented import as part of it, and 44
        # real anchors here -- `let provider = AnthropicProvider::new("key");`
        # among them -- were rejected as imports (FIG-1223).
        self.assertIn("is an import", import_anchor_defect(self.anchor(115)) or "")
        self.assertIsNone(import_anchor_defect(self.anchor(120)))
        self.assertIsNone(import_anchor_defect(self.anchor(121)))

    def test_keeps_a_use_site_that_merely_mentions_a_path(self):
        self.assertIsNone(
            import_anchor_defect(
                "crates/lash/src/session.rs:391#pub(crate) inner: "
                "lash_core::facade_support::ParkedSession,"
            )
        )

    def test_accepts_a_trait_impl_signature_as_a_dependency_claim(self):
        # Ruled for FIG-1223: implementing the contract is the strongest form the
        # dependency claim takes, so a signature inside `impl Trait for Type` is
        # evidence even though it declares rather than calls.
        self.assertIsNone(
            member_anchor_defect(
                "lash_core::facade_support::ToolStateFacadeOps::generation",
                [],
                "function",
                set(),
                self.anchor(108),
            )
        )

    def test_rejects_an_anchor_in_the_crate_that_declares_the_item(self):
        # The ledger keys an item by the path a host writes, so a re-export can
        # put the declaring crate outside the path root: `lash-sansio` declares
        # `lash_core::PreparedTurnMachine`.
        self.assertEqual(declaring_crates({"Env"}), {"crates/lash-fixture"})
        defect = declaration_anchor_defect(
            "lash_core::sansio::Env",
            "crates/lash-fixture/src/other.rs:3#let env = Env::default();",
        )
        self.assertIn("the crate whose source declares Env", defect or "")

    def test_rejects_a_bare_name_that_reaches_nothing(self):
        # The branch that accepted a field name inside a SQL string and a `let`
        # of the same name in an unrelated function.
        defect = self.defect("lash::triggers::TriggerFilter::attempt", 28, kind="field")
        self.assertIsNotNone(defect)
        self.assertIn("without reaching the member", defect)


class ProseCitationTests(unittest.TestCase):
    """Prose that cites a symbol has to be about the item inside that symbol."""

    FILE = "crates/lash-fixture/src/citation.rs"
    SOURCE = [
        "fn unrelated() {",
        "    let total = 1 + 1;",
        "}",
        "fn consumer(record: TurnRecord) {",
        "    let _ = record.duration_ms;",
        "}",
    ]

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        _RESOLVED_RECEIVERS.clear()
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)

    def test_accepts_a_citation_the_named_item_appears_in(self):
        self.assertIsNone(
            prose_citation_defect(
                "lash::TurnRecord::duration_ms",
                "field",
                f"Consumed at {self.FILE}::consumer#`let _ = record.duration_ms;` "
                "(FIG-1223).",
            )
        )

    def test_rejects_a_citation_that_never_mentions_the_item(self):
        defect = prose_citation_defect(
            "lash::TurnRecord::duration_ms",
            "field",
            f"Consumed at {self.FILE}::unrelated#`let total = 1 + 1;` (FIG-1223).",
        )
        self.assertIn("where neither duration_ms", defect or "")

    def test_rejects_a_symbol_the_file_no_longer_declares(self):
        # The break the ruling wants loud: the symbol moved file or died.
        defect = prose_citation_defect(
            "lash::TurnRecord::duration_ms",
            "field",
            f"Consumed at {self.FILE}::departed (FIG-1223).",
        )
        self.assertIn("declares no such symbol", defect or "")

    def test_rejects_a_snippet_that_left_its_symbol(self):
        defect = prose_citation_defect(
            "lash::TurnRecord::duration_ms",
            "field",
            f"Consumed at {self.FILE}::consumer#`let _ = record.retired_field;`.",
        )
        self.assertIn("does not appear there", defect or "")

    def test_rejects_a_file_scope_snippet_that_does_not_name_the_item(self):
        # A citation naming no symbol has the whole file as its window, and a
        # file that mentions the item somewhere would answer for a snippet about
        # anything.  This is the guarantee FIG-1526 won when it stopped reading
        # the file as an unenclosed line's scope, and the snippet now carries it.
        self.assertIn(
            "confirmed by its snippet or not at all",
            prose_citation_defect(
                "lash::TurnRecord::duration_ms",
                "field",
                f"Consumed at {self.FILE}#`let total = 1 + 1;`.",
            )
            or "",
        )
        self.assertIsNone(
            prose_citation_defect(
                "lash::TurnRecord::duration_ms",
                "field",
                f"Consumed at {self.FILE}#`let _ = record.duration_ms;`.",
            )
        )

    def test_rejects_a_line_pinned_citation_outright(self):
        defect = prose_citation_defect(
            "lash::TurnRecord::duration_ms", "field", f"Consumed at {self.FILE}:5."
        )
        self.assertIn("by line number", defect or "")

    def test_accepts_a_symbol_two_impl_blocks_both_answer_to(self):
        # More than one span is not a defect: the symbol is the anchor, and a
        # snippet found in either of them is found.
        _SOURCE_LINES[self.FILE] = [
            "impl Reader {",
            "    fn read(record: TurnRecord) {",
            "        let _ = record.duration_ms;",
            "    }",
            "}",
            "impl Writer {",
            "    fn read(record: TurnRecord) {",
            "        let _ = record.duration_ms + 1;",
            "    }",
            "}",
        ]
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        self.assertIsNone(
            prose_citation_defect(
                "lash::TurnRecord::duration_ms",
                "field",
                f"Consumed at {self.FILE}::read#`let _ = record.duration_ms + 1;`.",
            )
        )

    def test_holds_no_opinion_about_prose_without_a_citation(self):
        self.assertIsNone(
            prose_citation_defect("lash::TurnRecord", "struct", "Internal seam (FIG-1223).")
        )

    def test_rejects_a_snippet_carrying_no_code(self):
        # The whole point of FIG-1526, carried over: 588 citations landed on a
        # brace, an attribute or a comment and read as located facts because the
        # function around them happened to name the item.
        for snippet in ("}", "// duration_ms is read above", "#[test]"):
            self.assertIn(
                "carries no code",
                prose_citation_defect(
                    "lash::TurnRecord::duration_ms",
                    "field",
                    f"Consumed at {self.FILE}::consumer#`{snippet}`.",
                )
                or "",
                snippet,
            )

    def test_rejects_a_citation_to_a_blank_line_inside_a_naming_function(self):
        _SOURCE_LINES[self.FILE] = [
            "fn consumer(record: TurnRecord) {",
            "",
            "    let _ = record.duration_ms;",
            "}",
        ]
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        defect = prose_citation_defect(
            "lash::TurnRecord::duration_ms", "field", f"Consumed at {self.FILE}::consumer#`  `."
        )
        self.assertIn("carries no code", defect or "")

    def test_rejects_a_citation_to_a_closing_brace_or_a_comment(self):
        for snippet in ("}", "// duration_ms is read above"):
            _SOURCE_LINES[self.FILE] = [
                "fn consumer(record: TurnRecord) {",
                "    let _ = record.duration_ms;",
                "}",
                "fn other(record: TurnRecord) {",
                "    let _ = record.duration_ms;",
                "    // duration_ms is read above",
                "}",
            ]
            _SCOPE_BLOCKS.pop(self.FILE, None)
            _ANCHOR_SCOPES.clear()
            _SYMBOL_BLOCKS.clear()
            _CONSUMING_LINES.clear()
            defect = prose_citation_defect(
                "lash::TurnRecord::duration_ms",
                "field",
                f"Consumed at {self.FILE}::other#`{snippet}`.",
            )
            self.assertIn("carries no code", defect or "")

    def test_reads_prose_that_names_the_function_against_the_real_one(self):
        read = f"{self.FILE}::consumer#`let _ = record.duration_ms;`"
        defect = prose_citation_defect(
            "lash::TurnRecord::duration_ms",
            "field",
            f"The read at {read} in `unrelated` is the only one.",
        )
        self.assertIn("places", defect or "")
        self.assertIn("not part of the symbol", defect or "")
        self.assertIsNone(
            prose_citation_defect(
                "lash::TurnRecord::duration_ms",
                "field",
                f"The read at {read} in `consumer` is the only one.",
            )
        )

    def test_accepts_the_rows_own_anchor_a_test_function_never_names(self):
        anchors = {(self.FILE, 2)}
        reason = (
            f"the assertion at {self.FILE}::unrelated#`let total = 1 + 1;` "
            "in `unrelated` observes the total."
        )
        self.assertIn(
            "where neither duration_ms",
            prose_citation_defect("lash::TurnRecord::duration_ms", "field", reason) or "",
        )
        self.assertIsNone(
            prose_citation_defect(
                "lash::TurnRecord::duration_ms", "field", reason, anchors
            )
        )


class NameCoincidenceCitationTests(unittest.TestCase):
    """Spelling a leaf is not using it: prose and local names spell it too.

    Leaf names are ordinary words -- `deferred`, `abort`, `cancellation` -- so a
    sentence in an `.expect(...)` and a `let mut abort = None;` both put the
    word on a line that has nothing to do with the item.  Fifty-one citations
    rested on exactly that coincidence.
    """

    FILE = "crates/lash-fixture/src/coincidence.rs"
    SOURCE = [
        "fn narrates() {",
        '    let _ = value.expect("a deferred digest keeps its bytes");',
        "}",
        "fn binds() {",
        "    let mut deferred = None;",
        "    record(&mut deferred);",
        "}",
        "fn observes(report: ProcessDrainReport) {",
        "    let _ = report.deferred;",
        '    assert_eq!(projected["deferred"], 1);',
        "    // deferred is read above",
        "}",
    ]

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        _RESOLVED_RECEIVERS.clear()
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)
        self.addCleanup(_CONSUMING_LINES.clear)

    def test_rejects_a_line_that_spells_the_leaf_only_inside_a_sentence(self):
        defect = prose_citation_defect(
            "lash::durability::ProcessDrainReport::deferred",
            "field",
            f'Consumed at {self.FILE}::narrates#`let _ = value.expect("a deferred '
            'digest keeps its bytes");`.',
        )
        self.assertIn("a local name or literal text", defect or "")

    def test_rejects_a_line_that_spells_the_leaf_only_as_a_let_binder(self):
        defect = prose_citation_defect(
            "lash::durability::ProcessDrainReport::deferred",
            "field",
            f"Consumed at {self.FILE}::binds#`let mut deferred = None;`.",
        )
        self.assertIn("a local name or literal text", defect or "")

    def test_accepts_a_field_read_and_a_wire_key_that_names_the_field(self):
        for snippet in (
            "let _ = report.deferred;",
            'assert_eq!(projected["deferred"], 1);',
        ):
            self.assertIsNone(
                prose_citation_defect(
                    "lash::durability::ProcessDrainReport::deferred",
                    "field",
                    f"Consumed at {self.FILE}::observes#`{snippet}`.",
                ),
                snippet,
            )

    def test_rejects_a_wire_name_that_belongs_to_a_different_item(self):
        # "process.abandoned" is ProcessStatus::Abandoned's wire name, not this
        # field's: a dotted literal spells the leaf while naming another symbol.
        _SOURCE_LINES[self.FILE] = [
            "fn labels(status: ProcessStatus) -> &'static str {",
            '    ProcessStatus::Abandoned => "process.abandoned",',
            "}",
        ]
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        defect = prose_citation_defect(
            "lash::durability::ProcessDrainReport::abandoned",
            "field",
            f'Consumed at {self.FILE}::labels#`ProcessStatus::Abandoned => '
            '"process.abandoned",`.',
        )
        self.assertIn("a local name or literal text", defect or "")

    def test_masks_a_comment_without_swallowing_the_code_after_it(self):
        # A lone quote in a sentence is not a literal opening: reading it as one
        # blanks every line up to the next quote, and real uses vanish with it.
        _SOURCE_LINES[self.FILE] = [
            "fn observes(report: ProcessDrainReport) {",
            "    // the report doesn't say how many",
            "    let _ = report.deferred;",
            "}",
        ]
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        self.assertIsNone(
            prose_citation_defect(
                "lash::durability::ProcessDrainReport::deferred",
                "field",
                f"Consumed at {self.FILE}::observes#`let _ = report.deferred;`.",
            )
        )


class RejectedAnchorCitationTests(unittest.TestCase):
    """A row that cites the anchor it refuses is held to the inverse claim.

    Dropping the line number is the other way to keep such a row green, and it
    makes the rejected anchor unfindable -- which is how 24 used-unasserted rows
    went invisible in the first FIG-1526 round.
    """

    FILE = "crates/lash-fixture/src/rejected.rs"
    SOURCE = [
        "fn elsewhere() {",
        "    assert_eq!(session.id(), \"s-1\");",
        "}",
        "fn consumer(record: TurnRecord) {",
        "    assert_eq!(record.duration_ms, 4);",
        "}",
    ]

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)

    ELSEWHERE = 'assert_eq!(session.id(), "s-1");'
    CONSUMER = "assert_eq!(record.duration_ms, 4);"

    def defect(self, symbol, snippet, function):
        return prose_citation_defect(
            "lash::TurnRecord::duration_ms",
            "field",
            f"Downgraded: {self.FILE}::{symbol}#`{snippet}` asserts an unrelated "
            f"expression in `{function}`, and nothing else observes it.",
        )

    def test_accepts_an_assertion_that_says_nothing_about_the_item(self):
        self.assertIsNone(self.defect("elsewhere", self.ELSEWHERE, "elsewhere"))

    def test_rejects_a_rejection_the_function_contradicts(self):
        self.assertIn(
            "so the rejection is wrong",
            self.defect("consumer", self.CONSUMER, "consumer") or "",
        )

    def test_rejects_a_line_that_asserts_nothing(self):
        self.assertIn(
            "quotes no asserting snippet",
            self.defect("elsewhere", "fn elsewhere() {", "elsewhere") or "",
        )

    def test_rejects_a_rejection_placed_in_the_wrong_function(self):
        self.assertIn("places", self.defect("elsewhere", self.ELSEWHERE, "consumer") or "")


class UnresolvedCandidateCitationTests(unittest.TestCase):
    """A candidate the resolver cannot tie to the item still has to name it."""

    FILE = "crates/lash-fixture/src/candidate.rs"
    SOURCE = [
        "use lash_core::facade_support::ToolStateFacadeOps;",
        "",
        "trait ToolStateFacadeOps {",
        "    fn record_catalog(&self, catalog: Catalog);",
        "}",
        "",
        "fn consumer(state: ToolState) {",
        "    let ops: &dyn ToolStateFacadeOps = &state;",
        "    state.record_catalog(catalog);",
        "}",
    ]
    PROSE = (
        " as a consumer of this path and the checker cannot tie that citation to "
        "the owning type mechanically, so the row records the candidate."
    )

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)

    def defect(self, citation):
        return prose_citation_defect(
            "lash_core::facade_support::ToolStateFacadeOps",
            "trait",
            f"An earlier round named {self.FILE}{citation}{self.PROSE}",
        )

    def test_accepts_the_import_that_spells_the_trait(self):
        # A file-scope target has no enclosing symbol, so the file itself is the
        # anchor and the snippet carries the whole claim.
        self.assertIsNone(
            self.defect("#`use lash_core::facade_support::ToolStateFacadeOps;`")
        )

    def test_rejects_a_line_that_does_not_name_it(self):
        self.assertIn(
            "does not name it",
            self.defect("::consumer#`state.record_catalog(catalog);`") or "",
        )

    def test_reads_a_symbol_only_candidate_against_its_declaration_line(self):
        # The window is the narrowest honest one.  `consumer`'s body names the
        # trait, so reading the whole span would let a row whose resolver gave up
        # point at any function that happens to mention the item -- which is the
        # strictness the line-pinned form had and the symbol form must keep.
        self.assertIn("does not name it", self.defect("::consumer") or "")
        self.assertIsNone(self.defect("::ToolStateFacadeOps"))


class UnscopedProseCitationGateTests(unittest.TestCase):
    """The citation check runs on every row, not on the ones that admit to it.

    FIG-1223 ran it on internal dispositions and rows whose prose said
    "FIG-1223", so the way past it was to write the prose without the ticket.
    This drives `check` over a used-unasserted row that is neither, and it is red
    the moment that scope condition comes back.
    """

    FILE = "examples/docs-snippets/src/lib.rs"
    SOURCE = [
        "fn shows_the_turn(record: TurnRecord) {",
        "    let _ = record.duration_ms;",
        "}",
        "",
        "fn unrelated() {",
        "    let total = 1 + 1;",
        "}",
    ]
    CITATION = (
        "Read by the example at examples/docs-snippets/src/lib.rs::shows_the_turn"
        "#`let _ = record.duration_ms;`, inside `shows_the_turn`."
    )
    ITEM = ApiItem(
        primary="lash::TurnRecord::duration_ms",
        kind="field",
        availability="default+all-features",
        paths=["lash::TurnRecord::duration_ms"],
        identity="lash_core::runtime::turn_loop::TurnRecord::duration_ms",
    )

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)

    def run_check(self, reason, recorded=1):
        document = {
            "prose_citations_recorded": recorded,
            "api": [
                {
                    "symbol": "lash::TurnRecord::duration_ms",
                    "kind": "field",
                    "availability": "default+all-features",
                    "area": "sessions-turns",
                    "disposition": "used-unasserted",
                    "usage": f"{self.FILE}:2#let _ = record.duration_ms;",
                    "reason": reason,
                }
            ],
            "low_level_api": [],
            "removal_verdict": [],
            "removal_verdicts_recorded": 0,
            "gated_core_modules": [],
        }
        module = check_api_example_coverage
        with mock.patch.object(module, "inventory_document", lambda: document), mock.patch.object(
            module, "current_surface", lambda: [self.ITEM]
        ), mock.patch.object(
            module, "crate_directories", lambda: {"lash": "crates/lash"}
        ), mock.patch.object(
            module, "facade_dependency_dirs", set
        ), mock.patch.object(
            module, "REQUIRED_LOW_LEVEL_API", set()
        ), mock.patch.object(
            module, "EXAMPLE_TEST_TIER_RATCHET", 0
        ):
            errors, output = io.StringIO(), io.StringIO()
            with contextlib.redirect_stderr(errors), contextlib.redirect_stdout(output):
                code = check()
        return code, errors.getvalue()

    def test_fails_a_stale_citation_on_a_row_that_never_names_the_ticket(self):
        code, errors = self.run_check(
            f"Read by the example at {self.FILE}::unrelated#`let total = 1 + 1;`, "
            "which never mentions it."
        )
        self.assertEqual(code, 1, errors)
        self.assertIn("where neither duration_ms", errors)
        self.assertEqual(errors.count("\n- "), 1, errors)

    def test_passes_the_same_row_once_the_citation_lands_on_the_read(self):
        code, errors = self.run_check(self.CITATION)
        self.assertEqual(code, 0, errors)

    def test_rejects_a_line_pinned_citation(self):
        code, errors = self.run_check(
            f"Read by the example at {self.FILE}:2, inside `shows_the_turn`."
        )
        self.assertEqual(code, 1, errors)
        self.assertIn("by line number", errors)

    def test_rejects_dropping_the_symbol_that_makes_a_citation_checkable(self):
        # The evasion the check cannot see by itself: without `::shows_the_turn`
        # the reference is a file path, the citation pattern never matches it,
        # and the row is silently outside every rule above.
        code, errors = self.run_check(
            f"Read by the example at {self.FILE}, inside `shows_the_turn`."
        )
        self.assertEqual(code, 1, errors)
        self.assertIn("prose_citations_recorded is 1 but the reasons hold 0", errors)

    def test_requires_the_pin_to_move_when_a_citation_is_added(self):
        code, errors = self.run_check(self.CITATION, recorded=2)
        self.assertEqual(code, 1, errors)
        self.assertIn("hold 1 symbol citations", errors)


class LongSignatureScopeTests(unittest.TestCase):
    """A parameter list longer than the header walk left a function unfindable.

    Before FIG-1526 the walk stopped after fourteen lines, so a twenty-parameter
    constructor had no enclosing function -- harmless while a function-less line
    meant the whole file, and a citation-killer once it means nothing at all.
    """

    FILE = "crates/lash-fixture/src/long_signature.rs"
    SOURCE = [
        "impl ProcessEngineRunContext {",
        "    pub fn new(",
        *[f"        parameter_{index}: Parameter{index}," for index in range(20)],
        "    ) -> Self {",
        "        let processes = ProcessEngineProcessContext::new(registration);",
        "        Self { processes }",
        "    }",
        "}",
    ]

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)

    def test_finds_the_function_behind_a_twenty_parameter_signature(self):
        body = len(self.SOURCE) - 3
        self.assertIn("ProcessEngineProcessContext::new", anchor_scope(self.FILE, body))
        self.assertIsNone(
            prose_citation_defect(
                "lash_core::facade_support::ProcessEngineProcessContext",
                "struct",
                f"Exercised at {self.FILE}::ProcessEngineRunContext::new"
                "#`let processes = ProcessEngineProcessContext::new(registration);` "
                "in `new`.",
            )
        )


class AnchorTierTests(unittest.TestCase):
    """The tier is the anchor's path shape, never the row's claim about it."""

    def test_reads_another_crates_src_as_the_internal_tier(self):
        self.assertEqual(
            anchor_tier("crates/lash-restate/src/lib.rs:588#use lash_core::X;"),
            "crate-src",
        )
        self.assertEqual(
            anchor_crate("crates/lash-restate/src/lib.rs:588#use lash_core::X;"),
            "crates/lash-restate",
        )

    def test_reads_a_crate_tests_directory_as_the_weakest_tier(self):
        self.assertEqual(
            anchor_tier("crates/lash-core/tests/probe.rs:12#let _ = X;"),
            "workspace-tests",
        )

    def test_rejects_a_path_outside_the_anchorable_roots(self):
        self.assertIsNone(anchor_tier("docs/adr/0051.md:12#X"))
        self.assertIsNone(anchor_tier("not an anchor"))


class ExampleTestRatchetTests(unittest.TestCase):
    """The example-test population may fall; it may not rise."""

    def rows(self, count):
        # `benches/` needs no filesystem read to land in the example-test tier.
        return [
            {
                "disposition": "used-unasserted",
                "usage": f"examples/docs-snippets/benches/b.rs:{index + 1}#let _ = X;",
            }
            for index in range(count)
        ]

    def test_rejects_growth_past_the_ratchet(self):
        errors = example_test_tier_errors(self.rows(EXAMPLE_TEST_TIER_RATCHET + 1))
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("above the", errors[0])

    def test_requires_the_ratchet_to_follow_a_reduction_down(self):
        errors = example_test_tier_errors(self.rows(EXAMPLE_TEST_TIER_RATCHET - 1))
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("Lower EXAMPLE_TEST_TIER_RATCHET", errors[0])

    def test_accepts_the_recorded_population(self):
        self.assertEqual(example_test_tier_errors(self.rows(EXAMPLE_TEST_TIER_RATCHET)), [])

    def test_reports_the_distribution_every_run(self):
        lines = tier_breakdown(self.rows(2))
        self.assertIn("evidence tiers (rows by usage anchor):", lines[0])
        self.assertTrue(any("example-test: 2" in line for line in lines))
        self.assertTrue(any("ratchet" in line for line in lines))


class InternalConsumerTests(unittest.TestCase):
    """An `internal-consumed` anchor has to name a consumer, not the definition."""

    ITEM = ApiItem(
        primary="lash::TurnOutcome",
        kind="struct",
        availability="default+all-features",
        paths=["lash::TurnOutcome", "lash_core::facade_support::TurnOutcome"],
        identity="lash_core::runtime::turn_loop::TurnOutcome",
    )
    CRATE_DIRS = {"lash": "crates/lash", "lash_core": "crates/lash-core"}

    def errors(self, usage):
        rows = {
            ("lash::TurnOutcome", "struct"): {
                "symbol": "lash::TurnOutcome",
                "disposition": "internal-consumed",
                "usage": usage,
            }
        }
        return internal_consumer_errors(rows, [self.ITEM], self.CRATE_DIRS)

    def test_rejects_an_anchor_in_the_crate_that_defines_the_item(self):
        # The identity, not the path, names lash-core here.
        errors = self.errors("crates/lash-core/src/runtime/turn_loop.rs:88#TurnOutcome {")
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("which defines this item", errors[0])

    def test_accepts_an_anchor_in_a_consuming_crate(self):
        self.assertEqual(
            self.errors("crates/lash-restate/src/lib.rs:588#let outcome: TurnOutcome ="), []
        )


class InternalReferenceRelocationTests(unittest.TestCase):
    """Internal anchors survive documentation-only line movement."""

    def test_relocates_the_exact_source_within_the_recorded_file(self):
        module = check_api_example_coverage
        relative = "crates/lash/src/relocated_fixture.rs"
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            path = repo / relative
            path.parent.mkdir(parents=True)
            path.write_text(
                "/// Newly added docs.\n\nlet value = lash_core::Thing::new();\n",
                encoding="utf-8",
            )
            stale = f"{relative}:1#let value = lash_core::Thing::new();"
            with mock.patch.object(module, "REPO", repo), mock.patch.dict(
                module._SOURCE_LINES, {}, clear=True
            ):
                self.assertEqual(
                    resolved_internal_reference(stale),
                    f"{relative}:3#let value = lash_core::Thing::new();",
                )

    def test_rejects_an_anchor_whose_source_text_disappeared(self):
        module = check_api_example_coverage
        relative = "crates/lash/src/relocated_fixture.rs"
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            path = repo / relative
            path.parent.mkdir(parents=True)
            path.write_text("let replacement = NewThing::new();\n", encoding="utf-8")
            stale = f"{relative}:1#let value = lash_core::Thing::new();"
            with mock.patch.object(module, "REPO", repo), mock.patch.dict(
                module._SOURCE_LINES, {}, clear=True
            ):
                self.assertIsNone(resolved_internal_reference(stale))


class RemovalVerdictTests(unittest.TestCase):
    """FIG-1223: relocation cannot discharge a removal verdict.

    `678d567bf` is the shape under test. `SessionPickerInfo` carried a written
    `unused-remove` verdict, the commit moved it behind a doc-hidden module and
    deleted the row, and the gate had nothing left to compare against.
    """

    PICKER = ApiItem(
        primary="lash_core::facade_support::SessionPickerInfo",
        kind="struct",
        availability="default+all-features",
        paths=["lash_core::facade_support::SessionPickerInfo"],
        identity="lash_core::session::SessionPickerInfo",
    )

    def verdict(self, **extra):
        return {"symbol": "lash_core::SessionPickerInfo", "kind": "struct", **extra}

    def rows(self, disposition):
        return {
            ("lash_core::facade_support::SessionPickerInfo", "struct"): {
                "symbol": "lash_core::facade_support::SessionPickerInfo",
                "disposition": disposition,
            }
        }

    def errors(self, verdicts, recorded=None, rows=None, items=None):
        return removal_verdict_errors(
            verdicts,
            len(verdicts) if recorded is None else recorded,
            {} if rows is None else rows,
            [] if items is None else items,
        )

    def test_rejects_a_verdict_discharged_by_relocation(self):
        errors = self.errors(
            [self.verdict()], rows=self.rows("internal-consumed"), items=[self.PICKER]
        )
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("Relocation does not discharge", errors[0])
        self.assertIn("lash_core::facade_support::SessionPickerInfo", errors[0])

    def test_accepts_a_relocation_with_an_explicit_superseding_verdict(self):
        errors = self.errors(
            [
                self.verdict(
                    superseded_by="internal-consumed",
                    reason="Another crate's shipped code consumes it; anchor recorded.",
                )
            ],
            rows=self.rows("internal-consumed"),
            items=[self.PICKER],
        )
        self.assertEqual(errors, [])

    def test_rejects_a_superseding_verdict_the_ledger_disagrees_with(self):
        errors = self.errors(
            [self.verdict(superseded_by="used-asserted", reason="Exercised now.")],
            rows=self.rows("internal-consumed"),
            items=[self.PICKER],
        )
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("the tombstone and the ledger have to agree", errors[0])

    def test_requires_a_reason_for_a_superseding_verdict(self):
        errors = self.errors(
            [self.verdict(superseded_by="internal-consumed")],
            rows=self.rows("internal-consumed"),
            items=[self.PICKER],
        )
        self.assertTrue(any("requires a reason" in error for error in errors), errors)

    def test_accepts_a_relocation_that_keeps_the_verdict(self):
        # A path change is not laundering while the answer is unchanged: the
        # removal is still owed, at whatever path the item now uses.
        errors = self.errors(
            [
                self.verdict(),
                {"symbol": self.PICKER.primary, "kind": "struct"},
            ],
            rows=self.rows("unused-remove"),
            items=[self.PICKER],
        )
        self.assertEqual(errors, [])

    def test_accepts_a_verdict_discharged_by_actual_removal(self):
        self.assertEqual(self.errors([self.verdict()]), [])

    def test_requires_a_tombstone_for_every_removal_row(self):
        rows = {
            ("lash::Widget", "struct"): {"symbol": "lash::Widget", "disposition": "unused-remove"}
        }
        errors = self.errors([], rows=rows)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("without a removal verdict", errors[0])

    def test_pins_the_recorded_count_so_history_cannot_be_dropped(self):
        errors = self.errors([self.verdict()], recorded=2)
        self.assertEqual(len(errors), 1, errors)
        self.assertIn("removal_verdicts_recorded", errors[0])

    def test_keys_a_member_verdict_on_the_item_that_owns_it(self):
        # `id` alone is a hundred fields; the verdict is about one of them.
        self.assertEqual(
            relocation_key("lash_core::AcceptedInjectedTurnInput::id", "field"),
            "AcceptedInjectedTurnInput::id",
        )
        self.assertEqual(relocation_key("lash_core::AgentFrameRun", "struct"), "AgentFrameRun")


class RustdocIsolationTests(unittest.TestCase):
    """FIG-1823: the document this gate reads is the one this gate asked for.

    `<target>/doc/<crate>.json` is written by every documentation build in a
    checkout, so the gate builds under a subdirectory of its own and reads its
    document from there. The cache key must not move with it: it names the
    command, and the command is what decides the document.
    """

    def rustdoc(self, **environment):
        """Run `rustdoc()` against a stubbed cache, returning what it saw."""
        seen = {}
        served = Path(self.enterContext(tempfile.TemporaryDirectory())) / "document.json"
        served.write_text("{}", encoding="utf-8")

        def ensure(*, repo, package, crate_name, command, destination, generate):
            seen["command"] = list(command)
            seen["destination"] = destination
            generate()
            return served

        def run_command(command, *, cwd, env=None):
            seen["env"] = dict(env or {})
            seen["cwd"] = cwd

        with mock.patch.dict(os.environ, environment, clear=False):
            with mock.patch.object(
                check_api_example_coverage.rustdoc_json_cache, "ensure", ensure
            ), mock.patch.object(
                check_api_example_coverage.rustdoc_json_cache, "run_command", run_command
            ):
                check_api_example_coverage.rustdoc("lash-core", "lash_core", False)
        return seen

    def test_builds_under_a_subdirectory_of_the_checkouts_target_directory(self):
        self.assertEqual(
            check_api_example_coverage.GATE_TARGET,
            check_api_example_coverage.TARGET / "coverage-gate",
        )
        self.assertTrue(check_api_example_coverage.GATE_TARGET.is_absolute())

    def test_generation_runs_cargo_at_the_repository_root(self):
        # The only reason resolving a relative `CARGO_TARGET_DIR` against the
        # repository is right: cargo resolves it against its own working
        # directory, and that is where this gate puts cargo. Move generation to
        # another cwd and the destination stops naming cargo's output.
        self.assertEqual(self.rustdoc()["cwd"], check_api_example_coverage.REPO)

    def test_a_relative_target_directory_is_resolved_against_the_repository(self):
        # Where cargo lands a relative value, given the cwd pinned above.
        repo = check_api_example_coverage.REPO
        self.assertEqual(
            check_api_example_coverage.target_directory({"CARGO_TARGET_DIR": "shared-target"}),
            repo / "shared-target",
        )
        self.assertEqual(check_api_example_coverage.target_directory({}), repo / "target")
        self.assertEqual(
            check_api_example_coverage.target_directory({"CARGO_TARGET_DIR": "/elsewhere"}),
            Path("/elsewhere"),
        )

    def test_generation_and_the_read_share_the_isolated_target_directory(self):
        seen = self.rustdoc()
        self.assertEqual(
            seen["env"]["CARGO_TARGET_DIR"], str(check_api_example_coverage.GATE_TARGET)
        )
        self.assertEqual(
            seen["destination"],
            check_api_example_coverage.GATE_TARGET / "doc" / "lash_core.json",
        )

    def test_the_isolation_never_reaches_the_cache_key(self):
        # An absolute target path on the command line would key every checkout
        # separately and cost the cache its cross-worktree reuse.
        command = self.rustdoc()["command"]
        self.assertEqual(
            command,
            [
                "cargo",
                "rustdoc",
                "-p",
                "lash-core",
                "--lib",
                "--",
                "-Z",
                "unstable-options",
                "--output-format",
                "json",
                "--document-hidden-items",
            ],
        )
        self.assertFalse([argument for argument in command if "coverage-gate" in argument])

    def test_an_ambient_target_directory_does_not_leak_into_generation(self):
        seen = self.rustdoc(CARGO_TARGET_DIR="/somewhere/else")
        self.assertEqual(
            seen["env"]["CARGO_TARGET_DIR"], str(check_api_example_coverage.GATE_TARGET)
        )


class LowLevelRowsRunTheSameValidator(unittest.TestCase):
    """The `low_level_api` table answers to the `api` table's row checks.

    Until FIG-1865 the low-level table was validated by a hand-copied 26-line
    loop that had drifted from the 190-line one beside it, so a low-level row
    could rest on a tautology, on an anchor that observed nothing, or on a
    second row for the same symbol.  These fixtures are the standing proof that
    both loops call one validator: nothing here patches the low-level branch
    specifically, and every case is stated twice where the tables can both hold
    it.
    """

    FILE = "examples/docs-snippets/src/low_level_fixture.rs"
    SOURCE = [
        "fn exercises_the_vm() {",                                       # 1
        "    let thing = Thing::new();",                                 # 2
        "    assert!(size_of::<Thing>() > 0);",                          # 3
        "    assert_eq!(thing.code(), \"ok\");",                         # 4
        "    assert_eq!(",                                               # 5
        "        thing.code(),",                                         # 6
        "        \"ok\"",                                                # 7
        "    );",                                                        # 8
        "    let doubled = thing",                                       # 9
        "        .map(|value| value + 1);",                              # 10
        "    assert!(doubled.iter().all(|value| value > 0));",           # 11
        "}",                                                             # 12
    ]
    ITEM = ApiItem(
        primary="lash::Thing",
        kind="struct",
        availability="default+all-features",
        paths=["lash::Thing"],
        identity="lash_core::thing::Thing",
    )

    def setUp(self):
        _SOURCE_LINES[self.FILE] = list(self.SOURCE)
        _SCOPE_BLOCKS.pop(self.FILE, None)
        _ANCHOR_SCOPES.clear()
        _SYMBOL_BLOCKS.clear()
        _CONSUMING_LINES.clear()
        _RESOLVED_RECEIVERS.clear()
        _IMPORTED_TYPES.pop(self.FILE, None)
        _TYPE_FACTS.clear()
        _LITERAL_STACKS.pop(self.FILE, None)
        self.addCleanup(_TYPE_FACTS.clear)
        self.addCleanup(_LITERAL_STACKS.pop, self.FILE, None)
        self.addCleanup(_SOURCE_LINES.pop, self.FILE, None)
        self.addCleanup(_SCOPE_BLOCKS.pop, self.FILE, None)
        self.addCleanup(_IMPORTED_TYPES.pop, self.FILE, None)

    def anchor(self, line):
        return f"{self.FILE}:{line}#{self.SOURCE[line - 1].strip()}"

    def low_level_row(self, symbol, usage, assertion):
        return {
            "symbol": symbol,
            "disposition": "used-asserted",
            "usage": usage,
            "assertion": assertion,
        }

    def api_row(self, symbol, usage, assertion):
        return {
            "symbol": symbol,
            "kind": "struct",
            "availability": "default+all-features",
            "area": "sessions-turns",
            "disposition": "used-asserted",
            "usage": usage,
            "assertion": assertion,
        }

    def run_check(self, api_rows=(), low_level_rows=(), items=()):
        document = {
            "prose_citations_recorded": 0,
            "api": list(api_rows),
            "low_level_api": list(low_level_rows),
            "removal_verdict": [],
            "removal_verdicts_recorded": 0,
            "gated_core_modules": [],
        }
        module = check_api_example_coverage
        required = {row["symbol"] for row in low_level_rows}
        with mock.patch.object(module, "inventory_document", lambda: document), mock.patch.object(
            module, "current_surface", lambda: list(items)
        ), mock.patch.object(
            module, "crate_directories", lambda: {"lash": "crates/lash"}
        ), mock.patch.object(
            module, "facade_dependency_dirs", set
        ), mock.patch.object(
            module, "REQUIRED_LOW_LEVEL_API", required
        ), mock.patch.object(
            module, "EXAMPLE_TEST_TIER_RATCHET", 0
        ):
            errors, output = io.StringIO(), io.StringIO()
            with contextlib.redirect_stderr(errors), contextlib.redirect_stdout(output):
                code = check()
        return code, errors.getvalue()

    def reported(self, errors):
        return {
            line[2:] for line in errors.splitlines() if line.startswith("- ")
        }

    def test_a_tautological_assertion_fails_on_a_low_level_row(self):
        code, errors = self.run_check(
            low_level_rows=[
                self.low_level_row(
                    "lashlang::Thing", self.anchor(4), self.anchor(3)
                )
            ]
        )
        self.assertEqual(code, 1, errors)
        self.assertIn("is a tautology", errors)
        self.assertEqual(len(self.reported(errors)), 1, errors)

    def test_an_uninformative_assertion_fails_on_a_low_level_row(self):
        code, errors = self.run_check(
            low_level_rows=[
                self.low_level_row(
                    "lashlang::Thing", self.anchor(4), self.anchor(5)
                )
            ]
        )
        self.assertEqual(code, 1, errors)
        self.assertIn("carries no operands and observes nothing", errors)
        self.assertEqual(len(self.reported(errors)), 1, errors)

    def test_an_unrelated_fluent_assertion_fails_on_a_low_level_row(self):
        code, errors = self.run_check(
            low_level_rows=[
                self.low_level_row(
                    "lashlang::Thing", self.anchor(10), self.anchor(11)
                )
            ]
        )
        self.assertEqual(code, 1, errors)
        self.assertIn(
            "inherits a closure operand that can observe an unrelated callback",
            errors,
        )
        self.assertEqual(len(self.reported(errors)), 1, errors)

    def test_a_sound_low_level_row_still_passes(self):
        code, errors = self.run_check(
            low_level_rows=[
                self.low_level_row(
                    "lashlang::Thing", self.anchor(4), self.anchor(4)
                )
            ]
        )
        self.assertEqual(code, 0, errors)

    def test_duplicate_low_level_rows_are_reported(self):
        code, errors = self.run_check(
            low_level_rows=[
                self.low_level_row(
                    "lashlang::Thing", self.anchor(4), self.anchor(4)
                ),
                self.low_level_row(
                    "lashlang::Thing", self.anchor(4), self.anchor(4)
                ),
            ]
        )
        self.assertEqual(code, 1, errors)
        self.assertEqual(
            self.reported(errors),
            {"duplicate inventory entry: lashlang::Thing"},
            errors,
        )

    def test_identical_anchors_produce_the_same_error_set_on_both_tables(self):
        usage, assertion = self.anchor(4), self.anchor(5)
        api_code, api_errors = self.run_check(
            api_rows=[self.api_row("lash::Thing", usage, assertion)],
            items=[self.ITEM],
        )
        low_code, low_errors = self.run_check(
            low_level_rows=[self.low_level_row("lash::Thing", usage, assertion)]
        )
        self.assertEqual(api_code, 1, api_errors)
        self.assertEqual(low_code, 1, low_errors)
        self.assertEqual(self.reported(api_errors), self.reported(low_errors))
        self.assertEqual(len(self.reported(low_errors)), 1, low_errors)

    def test_a_low_level_row_that_states_no_kind_skips_only_the_kind_checks(self):
        """Field absence, not the loop, is why a check does not run."""
        self.assertTrue(check_api_example_coverage.API_TABLE.states("kind"))
        self.assertFalse(check_api_example_coverage.LOW_LEVEL_TABLE.states("kind"))
        self.assertEqual(
            check_api_example_coverage.LOW_LEVEL_TABLE.absent_fields,
            frozenset({"kind", "area", "availability", "aliases"}),
        )
        code, errors = self.run_check(
            low_level_rows=[
                self.low_level_row(
                    "lashlang::Thing", self.anchor(4), self.anchor(4)
                )
            ]
        )
        self.assertEqual(code, 0, errors)
        # The same row in the `api` table, which does state those fields, is
        # held to them.
        code, errors = self.run_check(
            api_rows=[
                self.low_level_row(
                    "lash::Thing", self.anchor(4), self.anchor(4)
                )
            ],
            items=[self.ITEM],
        )
        self.assertEqual(code, 1, errors)
        self.assertIn("invalid availability", errors)
        self.assertIn("unknown area", errors)


class TheExampleTestTierRatchetCountsBothTables(unittest.TestCase):
    def test_the_pin_is_unchanged_by_the_shared_validator(self):
        self.assertEqual(EXAMPLE_TEST_TIER_RATCHET, 1951)


if __name__ == "__main__":
    unittest.main()
