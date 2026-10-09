#!/usr/bin/env python3
"""Keep AppendVec's unsafe tests in the local Miri gate."""

from pathlib import Path
import os
import subprocess
import json
import sys
import tempfile
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]


class AppendVecMiriTests(unittest.TestCase):
    def test_recipe_records_pinned_setup_twenty_seeds_and_propagates_failure(self):
        pin = tomllib.loads((ROOT / "scripts/miri-toolchain.toml").read_text())["toolchain"]
        with tempfile.TemporaryDirectory(dir=ROOT / ".buck2") as directory:
            temp = Path(directory)
            log = temp / "commands.jsonl"
            recorder = "#!" + sys.executable + "\n" + """
import json, os, sys
from pathlib import Path
with Path(os.environ["RECIPE_LOG"]).open("a") as log:
    log.write(json.dumps({"tool": Path(sys.argv[0]).name, "args": sys.argv[1:],
                          "flags": os.environ.get("MIRIFLAGS")}) + "\\n")
if sys.argv[1:4] == [os.environ["RECIPE_CHANNEL"], "miri", "test"]:
    sys.exit(int(os.environ["RECIPE_FAILURE"]))
"""
            for name in ("rustup", "cargo"):
                tool = temp / name
                tool.write_text(recorder)
                tool.chmod(0o755)
            for failure in (0, 23):
                log.unlink(missing_ok=True)
                completed = subprocess.run(
                    ["bash", "scripts/append-vec-miri.sh"], cwd=ROOT,
                    env=os.environ | {"PATH": str(temp) + os.pathsep + os.environ["PATH"],
                        "RECIPE_LOG": str(log), "RECIPE_CHANNEL": "+" + pin["channel"],
                        "RECIPE_FAILURE": str(failure)}, capture_output=True, text=True,
                )
                with self.subTest(failure=failure):
                    self.assertEqual(completed.returncode, failure, completed.stderr)
                    commands = [json.loads(line) for line in log.read_text().splitlines()]
                    self.assertEqual([(row["tool"], row["args"]) for row in commands], [
                        ("rustup", ["toolchain", "install", pin["channel"], "--profile", pin["profile"],
                                    "--component", ",".join(pin["components"]), "--no-self-update"]),
                        ("cargo", ["+" + pin["channel"], "miri", "setup"]),
                        ("cargo", ["+" + pin["channel"], "miri", "test", "--locked", "-p",
                                   "lash-internal-sansio", "--lib", "--target-dir",
                                   str(ROOT / ".tgt/miri/target"), "append_vec::tests::", "--", "--test-threads=1"]),
                    ])
                    self.assertEqual(commands[2]["flags"], "-Zmiri-many-seeds=0..20")

    def test_driver_reaches_miri_without_requiring_buck2(self):
        result = subprocess.run(["bash", "scripts/hermetic-build.sh", "miri", "--help"], cwd=ROOT, env=os.environ | {"BUCK2": "append-vec-miri-buck2-must-not-be-used"}, capture_output=True, text=True)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("AppendVec", result.stdout)


if __name__ == "__main__":
    unittest.main()
