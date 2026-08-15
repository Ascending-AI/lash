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
        discovered = {
            path.parent.name
            for path in (ROOT / "runbooks").glob("*/runbook.md")
            if path.parent.name not in set(config["typescript_only"])
        }
        self.assertEqual(discovered, ordinary)
        for scenario in ordinary:
            self.assertIn((scenario, "lashlang"), actual)
            self.assertIn((scenario, "typescript"), actual)

    def test_shards_are_disjoint_and_complete(self) -> None:
        with MATRIX.MATRIX.open("rb") as handle:
            config = MATRIX.tomllib.load(handle)
        expected = MATRIX.rows(config)
        shards = [expected[offset::3] for offset in range(3)]
        self.assertEqual(sum(map(len, shards)), len(expected))
        self.assertEqual(
            {(row["scenario"], row["dialect"]) for row in expected},
            {
                (row["scenario"], row["dialect"])
                for shard in shards
                for row in shard
            },
        )


if __name__ == "__main__":
    unittest.main()
