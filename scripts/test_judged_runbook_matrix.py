import importlib.util
import pathlib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location(
    "judged_runbook_matrix", ROOT / "scripts" / "judged_runbook_matrix.py"
)
assert SPEC and SPEC.loader
MATRIX = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MATRIX)


SELF_CONTAINED_OPERATOR_RUNBOOKS = {
    "graceful-drain": ("03-observed.jsonl",),
    "request-abandon": ("03-observed.jsonl",),
    "session-lease-triage": (
        "01-facade-read-tests.log",
        "02-provider-hang.jsonl",
        "03-lease-takeover.jsonl",
        "04-commit-cas-livelock.jsonl",
        "08-direct-turn-recovery.jsonl",
    ),
    "version-bump-recreation": (
        "01-seed.jsonl",
        "02-refusal.jsonl",
        "03-recreation.jsonl",
        "04-health.jsonl",
    ),
}


def self_contained_operator_runbook_violations(
    runbooks: dict[str, str],
) -> list[str]:
    """Find external-page dependencies or evidence-free Phase 4 judgments."""
    violations = []
    for scenario, required_evidence in SELF_CONTAINED_OPERATOR_RUNBOOKS.items():
        text = runbooks[scenario]
        for deleted_page in ("docs/operations.html", "docs/PUBLISHING.md"):
            if deleted_page in text:
                violations.append(f"{scenario}: depends on deleted {deleted_page}")
        try:
            phase_four = text.split("## Phase 4", 1)[1].split("## Phase 5", 1)[0]
        except IndexError:
            violations.append(f"{scenario}: has no bounded Phase 4 judgment")
            continue
        if "Independent behavior evidence" not in phase_four:
            violations.append(f"{scenario}: Phase 4 does not distinguish behavior evidence")
        for artifact in required_evidence:
            if artifact not in phase_four:
                violations.append(f"{scenario}: Phase 4 omits {artifact}")
    return violations


class JudgedRunbookMatrixTests(unittest.TestCase):
    def operator_runbooks(self) -> dict[str, str]:
        return {
            scenario: (ROOT / "runbooks" / scenario / "runbook.md").read_text()
            for scenario in SELF_CONTAINED_OPERATOR_RUNBOOKS
        }

    def test_every_existing_runbook_has_exactly_one_typescript_row(self) -> None:
        # Discovery plus the row shape in one test: a runbook directory that
        # nobody classified is as invisible as a scenario that quietly emits a
        # second paid row under a language the tree no longer has.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        rows = MATRIX.rows(config)
        ordinary = set(config["scenarios"])
        excluded = (
            set(config["typescript_only"])
            | set(config["deterministic_only"])
            | set(config["no_rlm_session_only"])
        )
        discovered = {
            path.parent.name
            for path in (ROOT / "runbooks").glob("*/runbook.md")
            if path.parent.name not in excluded
        }
        self.assertEqual(discovered, ordinary)
        self.assertEqual(config["language"], "typescript")
        for scenario in ordinary:
            self.assertEqual(
                [row["label"] for row in rows if row["scenario"] == scenario],
                ["typescript"],
                f"`{scenario}` must emit exactly one row, labelled typescript",
            )

    def test_no_row_carries_a_retired_language_id(self) -> None:
        # ADR 0096 leaves one language id. A row labelled with the retired
        # surface would claim a session pin no host can serve, and the artifact
        # directory it names would be evidence of nothing.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        labels = {row["label"] for row in MATRIX.rows(config)}
        self.assertEqual(labels, {"typescript", "standard"})
        self.assertNotIn("dialects", config)
        self.assertNotIn("lashlang", MATRIX.MATRIX.read_text())

    def test_the_matrix_lists_no_scenario_twice(self) -> None:
        # A scenario in two groups is invisible to a per-group check while the
        # shard set silently grows, and every extra row is a paid judged run.
        # (TOML already refuses a repeated key inside one group.)
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        listed = [
            name
            for key in (
                "scenarios",
                "typescript_only",
                "deterministic_only",
                "no_rlm_session_only",
                "scripted_live_model",
            )
            for name in config.get(key, {})
        ]
        duplicates = sorted({name for name in listed if listed.count(name) > 1})
        self.assertEqual(duplicates, [], f"the matrix classifies {duplicates} twice")
        rows = MATRIX.rows(config)
        keys = [(row["scenario"], row["label"]) for row in rows]
        repeated = sorted({key for key in keys if keys.count(key) > 1})
        self.assertEqual(repeated, [], f"the matrix emits {repeated} more than once")

    def test_no_rlm_session_scenarios_get_one_mode_labelled_row(self) -> None:
        # A scenario that opens no RLM session has no language to pin, so its
        # row is labelled with the mode. Labelling it `typescript` would claim a
        # session pin the evidence cannot show.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        rows = MATRIX.rows(config)
        for scenario in config["no_rlm_session_only"]:
            emitted = [row for row in rows if row["scenario"] == scenario]
            self.assertEqual(
                [row["label"] for row in emitted],
                ["standard"],
                f"`{scenario}` must emit exactly one mode-labelled row",
            )
            self.assertNotIn(scenario, config["scenarios"])

    def test_scripted_live_model_scenarios_name_their_runner_and_no_language(
        self,
    ) -> None:
        # These rows are owned by a shell oracle, not by the judged shard, so
        # the runner is the only thing the matrix can be trusted on. A
        # per-entry language list would be a second source of truth for a
        # choice ADR 0096 removed.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        emitted = {row["scenario"] for row in MATRIX.rows(config)}
        for scenario, entry in config["scripted_live_model"].items():
            self.assertEqual(entry["runner"], "just rlm-smoke-e2e")
            self.assertNotIn("dialects", entry)
            self.assertNotIn(scenario, emitted)

    def test_the_row_total_is_the_stated_arithmetic(self) -> None:
        # The count is cited in the report, the runbook rules and the shard
        # plan. Deriving it here means a reclassification cannot silently leave
        # those citations stale.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        expected = (
            len(config["scenarios"])
            + len(config["typescript_only"])
            + len(config["no_rlm_session_only"])
        )
        self.assertEqual(len(MATRIX.rows(config)), expected)
        self.assertEqual(expected, 37)

    def test_every_scenario_declares_a_valid_tier_and_its_tier_model(self) -> None:
        # The tier word is what a reader trusts; the slug is what the bill is
        # for. A row whose model does not match its tier is a funding claim the
        # evidence cannot support, and nothing else in the repository looks.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        self.assertEqual(MATRIX.tier_violations(config), [])
        self.assertEqual(
            sorted(config["tiers"]), ["deterministic", "economy", "frontier"]
        )
        for item in MATRIX.rows(config):
            self.assertIn(item["tier"], config["tiers"])
            self.assertIn(item["model"], config["tiers"][item["tier"]])

    def test_a_mismatched_tier_model_is_rejected(self) -> None:
        # Drives the checker with the mutation it exists to catch, so an
        # assertion that only reads the shipped file cannot pass vacuously.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        config["no_rlm_session_only"]["request-abandon"]["model"] = config[
            "tiers"
        ]["frontier"][0]
        self.assertNotEqual(MATRIX.tier_violations(config), [])
        config["no_rlm_session_only"]["request-abandon"]["tier"] = "platinum"
        self.assertNotEqual(MATRIX.tier_violations(config), [])

    def test_no_deterministic_scenario_names_a_paid_model(self) -> None:
        # The tier's whole claim is that the row makes no provider network
        # call. A deterministic row pointing at a real slug would spend money
        # under a label that says it cannot.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        self.assertEqual(config["tiers"]["deterministic"], ["scripted-provider"])
        for item in MATRIX.rows(config):
            if item["tier"] == "deterministic":
                self.assertNotIn("/", item["model"])

    def test_shards_are_disjoint_and_complete(self) -> None:
        # Drives the script's own selection, so a regression in the shard
        # arithmetic turns this red. Re-implementing the split here tested
        # Python, not the script: an off-by-one that dropped one row of the total kept
        # this green.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        expected = MATRIX.rows(config)
        for count in (1, 3, 7):
            with self.subTest(count=count):
                shards = [
                    MATRIX.select_shard(expected, index, count)
                    for index in range(1, count + 1)
                ]
                self.assertEqual(
                    sum(map(len, shards)),
                    len(expected),
                    "sharding must be lossless and non-overlapping",
                )
                self.assertEqual(
                    [(row["scenario"], row["label"]) for row in expected],
                    sorted(
                        (
                            (row["scenario"], row["label"])
                            for shard in shards
                            for row in shard
                        ),
                        key=lambda key: [
                            (row["scenario"], row["label"]) for row in expected
                        ].index(key),
                    ),
                    "every row must appear in exactly one shard",
                )

    def test_shard_arguments_outside_the_range_are_refused(self) -> None:
        for bad in ("0/3", "4/3", "1/0", "x/3", "3"):
            with self.subTest(shard=bad):
                with self.assertRaises(Exception):
                    MATRIX.parse_shard(bad)
        self.assertEqual(MATRIX.parse_shard("2/3"), (2, 3))

    def test_operator_runbooks_are_self_contained_and_keep_observed_evidence(self) -> None:
        self.assertEqual(
            self_contained_operator_runbook_violations(self.operator_runbooks()), []
        )

    def test_deleted_operator_pages_have_no_documentation_or_example_consumers(self) -> None:
        candidates = [ROOT / "CONTRIBUTING.md"]
        candidates.extend((ROOT / "runbooks").glob("**/*.md"))
        candidates.extend((ROOT / "docs").glob("**/*.md"))
        candidates.extend((ROOT / "examples").glob("**/*.rs"))
        violations = []
        for path in candidates:
            text = path.read_text()
            for deleted_page in ("docs/operations.html", "docs/PUBLISHING.md"):
                if deleted_page in text:
                    violations.append(f"{path.relative_to(ROOT)}: {deleted_page}")
        self.assertEqual(violations, [])

    def test_operator_runbook_guard_rejects_external_and_evidence_free_judgment(
        self,
    ) -> None:
        # Exercise both failure classes without pinning the runbooks' prose. A
        # guard that only reads the shipped tree can stay green after its own
        # checks stop detecting the regression it exists to prevent.
        runbooks = self.operator_runbooks()
        runbooks["request-abandon"] += "\nRead docs/operations.html before scoring.\n"
        runbooks["version-bump-recreation"] = runbooks[
            "version-bump-recreation"
        ].replace("04-health.jsonl", "health-artifact")
        violations = self_contained_operator_runbook_violations(runbooks)
        self.assertTrue(any("depends on deleted" in item for item in violations))
        self.assertTrue(any("Phase 4 omits 04-health.jsonl" in item for item in violations))


if __name__ == "__main__":
    unittest.main()
