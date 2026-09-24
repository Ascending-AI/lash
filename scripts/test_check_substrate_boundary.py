#!/usr/bin/env python3

from pathlib import Path
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-substrate-boundary.sh"
ALLOWLIST = ROOT / "scripts/drive-determinism-allowlist.txt"
COUNT = ROOT / "scripts/drive-determinism-allowlist.count"


class DriveDeterminismRatchetTests(unittest.TestCase):
    def test_drive_determinism_rule_passes_on_the_tree(self) -> None:
        result = subprocess.run(
            ["bash", str(SCRIPT)],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_allowlist_only_shrinks(self) -> None:
        # Every entry pins one forbidden-construct site as `path:line # id`.
        # A fix deletes its lines; nothing may add lines, so the entry count
        # is capped by the committed count file.
        entries = [
            line
            for line in ALLOWLIST.read_text().splitlines()
            if line.strip() and not line.startswith("#")
        ]
        for entry in entries:
            path, _, line = entry.split("#")[0].strip().rpartition(":")
            with self.subTest(entry=entry):
                self.assertTrue(path.startswith("crates/"), entry)
                self.assertTrue(line.isdigit(), entry)
        cap = int(COUNT.read_text().strip())
        self.assertLessEqual(len(entries), cap)


if __name__ == "__main__":
    unittest.main()
