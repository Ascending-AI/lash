#!/usr/bin/env python3

import unittest

from check_api_example_coverage import (
    ApiItem,
    api_items,
    doc_hidden,
    impossible_facade_migration,
    item_errors,
    lash_core_surface,
    machine_local_path,
    missing_repository_path,
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
            "0": item("lash_core", "public", {"module": {"items": [1, 20]}}, 0),
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
        },
        "paths": {
            # `Handle` is exported as `lash_core::Handle` but defined in `h`, so
            # its identity is the definition path, not the export path.
            "2": {"path": ["lash_core", "h", "Handle"], "kind": "struct"},
            "6": {"path": ["lash_core", "private_mod", "Handed"], "kind": "struct"},
            "10": {"path": ["lash_core", "private_mod", "Hidden"], "kind": "struct"},
        },
    }


class DocHiddenTests(unittest.TestCase):
    def test_recognizes_both_recorded_attribute_shapes(self):
        self.assertTrue(doc_hidden({"attrs": ["#[doc(hidden)]"]}))
        self.assertTrue(doc_hidden({"attrs": ["#[doc( hidden )]"]}))
        self.assertTrue(doc_hidden({"attrs": [{"doc_hidden": None}]}))
        self.assertFalse(doc_hidden({"attrs": ["#[doc = \"hidden costs\"]"]}))
        self.assertFalse(doc_hidden({}))


class CoreSurfaceTests(unittest.TestCase):
    def setUp(self):
        self.surface = lash_core_surface(fixture(), False)

    def test_names_the_root_export_and_its_members(self):
        self.assertIn(("lash_core::Handle", "struct"), self.surface)
        self.assertIn(("lash_core::Handle::handed", "function"), self.surface)

    def test_enumerates_a_pub_crate_rooted_reachable_type_by_canonical_path(self):
        # FIG-937: the Session-class hole. `Handed` has no nameable path, but a
        # host holds the value and can call everything on it.
        self.assertIn(("lash_core::private_mod::Handed", "struct"), self.surface)
        self.assertIn(("lash_core::private_mod::Handed::slot", "field"), self.surface)
        self.assertIn(("lash_core::private_mod::Handed::callable", "function"), self.surface)

    def test_excludes_doc_hidden_members_and_what_only_they_expose(self):
        self.assertNotIn(("lash_core::Handle::hidden_handed", "function"), self.surface)
        self.assertNotIn(("lash_core::private_mod::Hidden", "struct"), self.surface)

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
        "#assert!(std::mem::size_of::<lash::triggers::TriggerIngressResult>() > 0);",
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


if __name__ == "__main__":
    unittest.main()
