#!/usr/bin/env python3
"""Regenerate runtime pins with named serialized-shape proofs through `kiln gate lash <fork> -- just runtime-commit-pins`."""

import json
from pathlib import Path
import re
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def main():
    files = (
        ROOT / "crates/lash-core/tests/runtime/tests/commit_bytes.rs",
        ROOT / "crates/lash-core/tests/runtime/tests/effect/commit_pins.rs",
    )
    scenarios = {
        scenario for path in files
        for scenario in re.findall(r'(?:assert_pinned|assert_commit_pins)\(\s*"([^"]+)"', path.read_text())
    }
    with tempfile.TemporaryDirectory(prefix="runtime-commit-pins.", dir=ROOT / ".buck2") as directory:
        for path, target, module in (
            (files[0], "//crates/lash-core:runtime_lifecycle__test", "runtime::tests::commit_bytes"),
            (files[1], "//crates/lash-core:runtime_effect__test", "runtime::tests::effect::commit_pins"),
        ):
            tests = re.findall(r"#\[tokio::test[^\n]*\]\s*async fn (\w+)", path.read_text())
            assert tests, f"{path}: no pin tests discovered"
            subprocess.run(
                ["kiln", "test", target, "--test_arg=--exact",
                 *(f"--test_arg={module}::{test}" for test in tests),
                 "--local-test-execution", "--nocache_test_results",
                 f"--test_env=LASH_RUNTIME_COMMIT_PIN_CAPTURE_DIR={directory}"],
                cwd=ROOT, check=True,
            )
        captures = [json.loads(path.read_text()) for path in Path(directory).glob("*.json")]
        assert len(captures) == len(scenarios), "every scenario must execute exactly once"
        assert {capture["scenario"] for capture in captures} == scenarios, "every scenario must execute"
        replacements = {}
        for capture in captures:
            for old, new, proof in zip(capture["expected"], capture["digests"], capture["proofs"], strict=True):
                assert proof["restored_digest"] == old, capture["scenario"]
                assert bool(proof["shape_changes"]) == (old != new), capture["scenario"]
                assert replacements.setdefault(old, new) == new, "a shared old pin diverged"
            print(f"{capture['scenario']}: {capture['proofs']}")
        generated = {}
        found = set()
        for path in files:
            source = path.read_text()
            for old, new in replacements.items():
                if f'"{old}"' in source:
                    found.add(old)
                    source = source.replace(f'"{old}"', f'"{new}"')
            generated[path] = source
        assert replacements and found == replacements.keys(), "every captured pin must have an owner"
        for path, source in generated.items():
            path.write_text(source)
        changed = sum(capture["digests"] != capture["expected"] for capture in captures)
        print(f"Captured all {len(captures)} scenarios; regenerated {changed}. Each changed pin reproduces its old digest by reversing only the reported serialized-shape changes.")


if __name__ == "__main__":
    main()
