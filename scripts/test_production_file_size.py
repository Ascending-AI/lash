#!/usr/bin/env python3

import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check-production-file-size.py").resolve()


class ProductionFileSizeGuardTests(unittest.TestCase):
    def run_guard(self, root: Path, *, production: int, test: int) -> subprocess.CompletedProcess[str]:
        env = os.environ.copy()
        env["LASH_PRODUCTION_RUST_LINE_LIMIT"] = str(production)
        env["LASH_TEST_RUST_LINE_LIMIT"] = str(test)
        return subprocess.run(
            ["python3", str(SCRIPT)],
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


    def test_exact_boundaries_doc_comments_and_unterminated_line(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source with spaces.rs"
            source.write_bytes(b"/// doc\n  //! inner\n\t/// tab\n// ordinary\n\nfn final() {}")
            at_limit = self.run_guard(root, production=3, test=10)
            over_limit = self.run_guard(root, production=2, test=10)
            self.assertEqual(at_limit.returncode, 0, at_limit.stderr)
            self.assertEqual(over_limit.returncode, 1)
            self.assertEqual(
                over_limit.stderr,
                "Rust files over line budget:\n  production limit: 2 lines\n"
                "  test/support limit: 10 lines\n  production:3:source with spaces.rs\n",
            )

    def test_test_support_classification_and_exclusions(self) -> None:
        test_paths = (
            "crates/lash-conformance/src/laws.rs", "crates/x/tests/a.rs",
            "crates/x/test/a.rs", "crates/x/testing/a.rs", "crates/x/src/tests.rs",
            "crates/x/src/test.rs", "crates/x/src/nested/tests.rs",
            "crates/x/src/nested/test.rs", "crates/x/src/parser_tests.rs",
            "crates/x/language/support.rs",
        )
        excluded = (".git", ".claude", "target", ".tgt", "vendor", "vendored", "generated", "crates/lash-regress")
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for relative in test_paths:
                source = root / relative
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text("one\ntwo\n")
            for relative in excluded:
                source = root / relative / "nested/over.rs"
                source.parent.mkdir(parents=True, exist_ok=True)
                source.write_text("over\n" * 20)
            passed = self.run_guard(root, production=1, test=2)
            self.assertEqual(passed.returncode, 0, passed.stderr)
            failed = self.run_guard(root, production=1, test=1)
            self.assertEqual(failed.returncode, 1)
            self.assertEqual(
                set(failed.stderr.splitlines()[3:]),
                {f"  test:2:{relative}" for relative in test_paths},
            )

    def test_explicit_roots_and_symlinks(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "actual.rs"
            source.write_text("line\n" * 1601)
            (root / "linked.rs").symlink_to(source)
            (root / "directory-link").symlink_to(root, target_is_directory=True)
            result = subprocess.run(
                ["python3", str(SCRIPT), str(root / "linked.rs"), str(source)],
                cwd=root, text=True, capture_output=True,
                env={key: value for key, value in os.environ.items() if key not in (
                    "LASH_PRODUCTION_RUST_LINE_LIMIT", "LASH_TEST_RUST_LINE_LIMIT")},
            )
            self.assertEqual(result.returncode, 1)
            self.assertEqual(result.stderr.count("production:1601:"), 1)
            self.assertIn(str(source), result.stderr)


if __name__ == "__main__":
    unittest.main()
