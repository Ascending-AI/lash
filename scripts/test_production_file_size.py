#!/usr/bin/env python3

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-production-file-size.sh").resolve()


class ProductionFileSizeGuardTests(unittest.TestCase):
    def run_guard(self, root: Path, *, production: int, test: int) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["LASH_PRODUCTION_RUST_LINE_LIMIT"] = str(production)
        env["LASH_TEST_RUST_LINE_LIMIT"] = str(test)
        return subprocess.run(
            ["bash", str(SCRIPT)],
            cwd=root,
            env=env,
            text=True,
            capture_output=True,
            check=False,
        )

    def test_formerly_exempt_production_path_fails_over_budget(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/lash-sim/src/oracles.rs"
            source.parent.mkdir(parents=True)
            source.write_text("fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n")

            result = self.run_guard(root, production=3, test=10)

            self.assertNotEqual(result.returncode, 0)
            self.assertIn("production:4:crates/lash-sim/src/oracles.rs", result.stderr)

    def test_test_tree_uses_the_larger_test_budget(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "crates/lash-restate/src/tests.rs"
            source.parent.mkdir(parents=True)
            source.write_text("fn one() {}\nfn two() {}\nfn three() {}\nfn four() {}\n")

            within_test_budget = self.run_guard(root, production=3, test=4)
            over_test_budget = self.run_guard(root, production=3, test=3)

            self.assertEqual(within_test_budget.returncode, 0, within_test_budget.stderr)
            self.assertNotEqual(over_test_budget.returncode, 0)
            self.assertIn("test:4:crates/lash-restate/src/tests.rs", over_test_budget.stderr)


if __name__ == "__main__":
    unittest.main()
