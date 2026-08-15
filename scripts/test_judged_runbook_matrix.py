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
        excluded = set(config["typescript_only"]) | set(config["deterministic_only"])
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
        # A duplicated scenario is invisible to a set comparison while the
        # shard set silently grows, and every extra row is a paid judged run.
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        for key in ("scenarios", "typescript_only", "deterministic_only"):
            listed = config.get(key, [])
            duplicates = sorted(
                {name for name in listed if listed.count(name) > 1}
            )
            self.assertEqual(duplicates, [], f"`{key}` repeats {duplicates}")
        rows = MATRIX.rows(config)
        keys = [(row["scenario"], row["dialect"]) for row in rows]
        repeated = sorted({key for key in keys if keys.count(key) > 1})
        self.assertEqual(repeated, [], f"the matrix emits {repeated} more than once")

    def test_shards_are_disjoint_and_complete(self) -> None:
        # Drives the script's own selection, so a regression in the shard
        # arithmetic turns this red. Re-implementing the split here tested
        # Python, not the script: an off-by-one that dropped one row of 65 kept
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
