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


class JudgedRunbookMatrixTests(unittest.TestCase):
    def test_every_existing_runbook_has_both_dialect_rows(self) -> None:
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        rows = MATRIX.rows(config)
        ordinary = set(config["scenarios"])
        actual = {(row["scenario"], row["dialect"]) for row in rows}
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
        for scenario in ordinary:
            self.assertIn((scenario, "lashlang"), actual)
            self.assertIn((scenario, "typescript"), actual)

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
            )
            for name in config.get(key, {})
        ]
        duplicates = sorted({name for name in listed if listed.count(name) > 1})
        self.assertEqual(duplicates, [], f"the matrix classifies {duplicates} twice")
        rows = MATRIX.rows(config)
        keys = [(row["scenario"], row["dialect"]) for row in rows]
        repeated = sorted({key for key in keys if keys.count(key) > 1})
        self.assertEqual(repeated, [], f"the matrix emits {repeated} more than once")

    def test_no_rlm_session_scenarios_get_one_dialect_neutral_row(self) -> None:
        # A scenario that opens no RLM session has no dialect to pin. A second
        # row would buy an identical judged run and label it with a language the
        # session never had.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        rows = MATRIX.rows(config)
        for scenario in config["no_rlm_session_only"]:
            emitted = [row for row in rows if row["scenario"] == scenario]
            self.assertEqual(
                [row["dialect"] for row in emitted],
                ["standard"],
                f"`{scenario}` must emit exactly one dialect-neutral row",
            )
            self.assertNotIn(scenario, config["scenarios"])

    def test_the_row_total_is_the_stated_arithmetic(self) -> None:
        # The count is cited in the report, the runbook rules and the shard
        # plan. Deriving it here means a reclassification cannot silently leave
        # those citations stale.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        expected = (
            len(config["scenarios"]) * len(config["dialects"])
            + len(config["typescript_only"])
            + len(config["no_rlm_session_only"])
        )
        self.assertEqual(len(MATRIX.rows(config)), expected)
        self.assertEqual(expected, 61)

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
        config["no_rlm_session_only"]["docs-quickstart"]["model"] = config[
            "tiers"
        ]["frontier"][0]
        self.assertNotEqual(MATRIX.tier_violations(config), [])
        config["no_rlm_session_only"]["docs-quickstart"]["tier"] = "platinum"
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
                    [(row["scenario"], row["dialect"]) for row in expected],
                    sorted(
                        (
                            (row["scenario"], row["dialect"])
                            for shard in shards
                            for row in shard
                        ),
                        key=lambda key: [
                            (row["scenario"], row["dialect"]) for row in expected
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


if __name__ == "__main__":
    unittest.main()
