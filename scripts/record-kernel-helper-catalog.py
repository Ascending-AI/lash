#!/usr/bin/env python3
"""Record the kernel helper catalog: python3 scripts/record-kernel-helper-catalog.py.

Builds the same catalog the release build generates, then copies it for review.
The helper classification recorder is:
kiln run //crates/lash-kernel-releases:record-helper-data__bin -- record_helper_classes
"""

import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools" / "buck2"))
from outputs import resolve


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--build-report", type=Path,
                        default=ROOT / ".kiln/helper-catalog/build-report.json")
    report = parser.parse_args().build_report.resolve()
    report.parent.mkdir(parents=True, exist_ok=True)
    label = "//crates/lash-kernel-releases:build_script"
    subprocess.run(
        ["kiln", "build", f"{label}[out_dir]", "--materializations", "final",
         "--build-report", str(report)],
        cwd=ROOT,
        check=True,
    )
    outputs = resolve(json.loads(report.read_text()), label, "out_dir")
    if len(outputs) != 1:
        raise ValueError(f"expected one build output directory, found {outputs}")
    destination = ROOT / "crates/lash-kernel-lib/src/helpers.rs"
    shutil.copyfile(Path(outputs[0]) / "kernel_helpers.rs", destination)
    print(f"recorded {destination.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
