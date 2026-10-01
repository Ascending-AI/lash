#!/usr/bin/env python3
"""Regenerate FIG-4666 runtime pins through `kiln gate lash <fork> -- just runtime-commit-pins`."""

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
        for target, test_filter in (
            ("//crates/lash-core:runtime_lifecycle__test", "commit_bytes::"),
            ("//crates/lash-core:runtime_effect__test", "commit_pins::"),
        ):
            subprocess.run(
                ["kiln", "test", target, f"--test_arg={test_filter}",
                 "--local-test-execution", "--nocache_test_results",
                 f"--test_env=LASH_RUNTIME_COMMIT_PIN_CAPTURE_DIR={directory}"],
                cwd=ROOT, check=True,
            )
        captures = [json.loads(path.read_text()) for path in Path(directory).glob("*.json")]
        assert {capture["scenario"] for capture in captures} == scenarios, "every scenario must execute"
        replacements = {}
        for capture in captures:
            assert capture["digests"] == capture["expected"] or capture["restored_frame_digests"] == capture["expected"], capture["scenario"]
            for old, new in zip(capture["expected"], capture["digests"], strict=True):
                assert replacements.setdefault(old, new) == new, "a shared old pin diverged"
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
        print(f"Captured all {len(captures)} scenarios; regenerated {changed}. Each changed pin differs only by the removed frame claim.")


if __name__ == "__main__":
    main()
